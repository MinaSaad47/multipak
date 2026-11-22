//! App icon lookup and badge rendering.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use resvg::{tiny_skia, usvg};

use crate::error::{Error, IoContext, Result};
use crate::instance::Badge;
use crate::paths::Paths;

/// Size of generated badge icons.
pub const BADGE_ICON_SIZE: u32 = 256;

const SIZES: &[&str] = &["scalable", "512x512", "256x256", "192x192", "128x128", "96x96", "64x64", "48x48", "32x32"];

/// Finds the best icon file for `app_id`: scalable SVG first, then the largest PNG, from
/// the Flatpak export folders and then the regular icon theme folders.
pub fn find_app_icon(paths: &Paths, app_id: &str) -> Option<PathBuf> {
    for share in paths.share_dirs() {
        let hicolor = share.join("icons/hicolor");
        for size in SIZES {
            for ext in ["svg", "png"] {
                if *size == "scalable" && ext == "png" {
                    continue;
                }
                let p = hicolor.join(size).join("apps").join(format!("{app_id}.{ext}"));
                if p.is_file() {
                    return Some(p);
                }
            }
        }
        let pixmap = share.join("pixmaps").join(format!("{app_id}.png"));
        if pixmap.is_file() {
            return Some(pixmap);
        }
    }
    None
}

fn fontdb() -> Arc<usvg::fontdb::Database> {
    static DB: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = usvg::fontdb::Database::new();
        db.load_system_fonts();
        // fontdb maps the generic `sans-serif` family to Arial, which most Linux systems
        // do not have. Point it at a sans font that is actually installed.
        let families: Vec<String> = db.faces().flat_map(|f| f.families.iter().map(|(name, _)| name.clone())).collect();
        let preferred = [
            "Noto Sans",
            "DejaVu Sans",
            "Cantarell",
            "Liberation Sans",
            "Ubuntu",
            "Roboto",
            "Open Sans",
            "Fira Sans",
            "Inter",
            "Arial",
        ];
        let chosen = preferred
            .iter()
            .find(|p| families.iter().any(|f| f == *p))
            .map(|s| s.to_string())
            .or_else(|| families.iter().find(|f| f.contains("Sans")).cloned())
            .or_else(|| families.first().cloned());
        if let Some(family) = chosen {
            db.set_sans_serif_family(family);
        }
        Arc::new(db)
    })
    .clone()
}

fn svg_options() -> usvg::Options<'static> {
    usvg::Options { fontdb: fontdb(), ..Default::default() }
}

/// Renders an SVG or PNG file into a square pixmap of `size` pixels, centered.
fn load_icon(source: &Path, size: u32) -> Result<tiny_skia::Pixmap> {
    let bytes = std::fs::read(source).ctx(|| format!("could not read {}", source.display()))?;
    let mut pixmap = tiny_skia::Pixmap::new(size, size).ok_or_else(|| Error::Icon("invalid size".into()))?;
    let is_svg = source.extension().is_some_and(|e| e.eq_ignore_ascii_case("svg"))
        || bytes.starts_with(b"<?xml")
        || bytes.starts_with(b"<svg");
    if is_svg {
        let tree = usvg::Tree::from_data(&bytes, &svg_options()).map_err(|e| Error::Icon(e.to_string()))?;
        let s = tree.size();
        let scale = (size as f32 / s.width()).min(size as f32 / s.height());
        let dx = (size as f32 - s.width() * scale) / 2.0;
        let dy = (size as f32 - s.height() * scale) / 2.0;
        resvg::render(&tree, tiny_skia::Transform::from_row(scale, 0.0, 0.0, scale, dx, dy), &mut pixmap.as_mut());
    } else {
        let img = image::load_from_memory(&bytes).map_err(|e| Error::Icon(format!("{}: {e}", source.display())))?;
        let img = img.resize(size, size, image::imageops::FilterType::Lanczos3).to_rgba8();
        let (w, h) = img.dimensions();
        let ox = (size - w) / 2;
        let oy = (size - h) / 2;
        let data = pixmap.data_mut();
        for (x, y, px) in img.enumerate_pixels() {
            let [r, g, b, a] = px.0;
            let idx = (((y + oy) * size + (x + ox)) * 4) as usize;
            // tiny-skia stores premultiplied alpha.
            let pm = |c: u8| ((u16::from(c) * u16::from(a) + 127) / 255) as u8;
            data[idx..idx + 4].copy_from_slice(&[pm(r), pm(g), pm(b), a]);
        }
    }
    Ok(pixmap)
}

/// Black or white, whichever reads better on `rgb`.
pub fn contrast_color(rgb: [u8; 3]) -> &'static str {
    let lin = |c: u8| {
        let c = f32::from(c) / 255.0;
        if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    };
    let l = 0.2126 * lin(rgb[0]) + 0.7152 * lin(rgb[1]) + 0.0722 * lin(rgb[2]);
    if l > 0.4 { "#000000" } else { "#ffffff" }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// SVG for the badge overlay (or a full tile when there is no source icon).
fn badge_svg(size: u32, badge: &Badge, full_tile: bool) -> Result<String> {
    let rgb = badge.rgb()?;
    let color = crate::instance::format_hex_color(rgb);
    let fg = contrast_color(rgb);
    let s = size as f32;
    let letter = badge.letter.as_deref().map(xml_escape).unwrap_or_default();
    let body = if full_tile {
        let r = s * 0.2;
        format!(
            r##"<rect x="{m}" y="{m}" width="{w}" height="{w}" rx="{r}" fill="{color}"/>
<text x="{c}" y="{c}" dy="0.36em" font-size="{fs}" font-family="sans-serif" font-weight="bold" text-anchor="middle" fill="{fg}">{letter}</text>"##,
            m = s * 0.06,
            w = s * 0.88,
            c = s / 2.0,
            fs = s * 0.55,
        )
    } else {
        let r = s * 0.22;
        let c = s - r - s * 0.02;
        format!(
            r##"<circle cx="{c}" cy="{c}" r="{r}" fill="{color}" stroke="#ffffff" stroke-width="{sw}"/>
<text x="{c}" y="{c}" dy="0.36em" font-size="{fs}" font-family="sans-serif" font-weight="bold" text-anchor="middle" fill="{fg}">{letter}</text>"##,
            sw = s * 0.025,
            fs = r * 1.25,
        )
    };
    Ok(format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{size}" height="{size}" viewBox="0 0 {size} {size}">{body}</svg>"##
    ))
}

/// Renders the app icon with a colored badge (and optional letter) in the bottom-right
/// corner and saves it as PNG. Without a source icon, a colored tile with the letter is
/// drawn instead.
pub fn render_badged_icon(source: Option<&Path>, badge: &Badge, out: &Path) -> Result<()> {
    let size = BADGE_ICON_SIZE;
    let (mut pixmap, full_tile) = match source {
        Some(src) => (load_icon(src, size)?, false),
        None => (tiny_skia::Pixmap::new(size, size).ok_or_else(|| Error::Icon("invalid size".into()))?, true),
    };
    let svg = badge_svg(size, badge, full_tile)?;
    let tree = usvg::Tree::from_str(&svg, &svg_options()).map_err(|e| Error::Icon(e.to_string()))?;
    resvg::render(&tree, tiny_skia::Transform::identity(), &mut pixmap.as_mut());
    let png = pixmap.encode_png().map_err(|e| Error::Icon(e.to_string()))?;
    crate::atomic::write_atomic(out, &png, 0o644)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="32"><rect width="64" height="32" fill="#00ff00"/></svg>"##;

    #[test]
    fn finds_icons_in_export_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::for_home(dir.path());
        assert_eq!(find_app_icon(&paths, "com.x.Y"), None);
        let png = paths.flatpak_system_dirs[0].join("exports/share/icons/hicolor/128x128/apps/com.x.Y.png");
        std::fs::create_dir_all(png.parent().unwrap()).unwrap();
        std::fs::write(&png, "x").unwrap();
        assert_eq!(find_app_icon(&paths, "com.x.Y"), Some(png));
        let svg = paths.flatpak_user_dir.join("exports/share/icons/hicolor/scalable/apps/com.x.Y.svg");
        std::fs::create_dir_all(svg.parent().unwrap()).unwrap();
        std::fs::write(&svg, SVG).unwrap();
        assert_eq!(find_app_icon(&paths, "com.x.Y"), Some(svg));
    }

    #[test]
    fn renders_badges() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("icon.svg");
        std::fs::write(&src, SVG).unwrap();
        let badge = Badge { color: "#e01b24".into(), letter: Some("W".into()) };
        let out = dir.path().join("out/badged.png");
        render_badged_icon(Some(&src), &badge, &out).unwrap();
        let img = image::open(&out).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (BADGE_ICON_SIZE, BADGE_ICON_SIZE));
        // Center of the source icon is green, badge area at bottom-right is red-ish.
        assert_eq!(img.get_pixel(128, 100).0, [0, 255, 0, 255]);
        let p = img.get_pixel(195, 195).0;
        assert!(p[0] > 200 && p[1] < 80, "{p:?}");

        let png_src = dir.path().join("icon.png");
        image::RgbaImage::from_pixel(48, 48, image::Rgba([0, 0, 255, 128])).save(&png_src).unwrap();
        render_badged_icon(Some(&png_src), &badge, &out).unwrap();
        let img = image::open(&out).unwrap().to_rgba8();
        let c = img.get_pixel(100, 100).0;
        assert!(c[2] > 240 && (120..=136).contains(&c[3]), "{c:?}");

        render_badged_icon(None, &Badge { color: "#ffffff".into(), letter: None }, &out).unwrap();
        assert_eq!(image::open(&out).unwrap().to_rgba8().get_pixel(128, 128).0, [255, 255, 255, 255]);
    }

    #[test]
    fn contrast() {
        assert_eq!(contrast_color([255, 255, 0]), "#000000");
        assert_eq!(contrast_color([30, 60, 120]), "#ffffff");
    }
}
