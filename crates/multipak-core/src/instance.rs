//! The instance model and all input validation.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::apptype::Strategy;
use crate::error::{Error, Result};
use crate::fsutil::paths_overlap;
use crate::paths::Paths;

/// Environment variable Multipak sets on every launch so running sandboxes can be
/// matched back to their instance.
pub const MARKER_ENV: &str = "MULTIPAK_INSTANCE";

pub const MAX_NAME_LEN: usize = 64;
pub const MAX_SLUG_LEN: usize = 48;
pub const MAX_WM_CLASS_LEN: usize = 128;

/// Uniquely identifies an instance: app ID plus slug.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct InstanceKey {
    pub app_id: String,
    pub slug: String,
}

impl InstanceKey {
    pub fn new(app_id: impl Into<String>, slug: impl Into<String>) -> Self {
        Self { app_id: app_id.into(), slug: slug.into() }
    }

    /// Parses `<app-id>/<slug>`.
    pub fn parse(s: &str) -> Option<Self> {
        let (app, slug) = s.rsplit_once('/')?;
        (validate_app_id(app).is_ok() && validate_slug(slug).is_ok()).then(|| Self::new(app, slug))
    }
}

impl fmt::Display for InstanceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.app_id, self.slug)
    }
}

/// A colored badge drawn on the instance's icon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Badge {
    /// `#rrggbb`
    pub color: String,
    /// One character shown inside the badge, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub letter: Option<String>,
}

impl Badge {
    pub fn rgb(&self) -> Result<[u8; 3]> {
        parse_hex_color(&self.color)
    }
}

/// A saved instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instance {
    pub app_id: String,
    /// Display name of the app when the instance was saved (used if the app is uninstalled).
    pub app_name: String,
    pub name: String,
    pub slug: String,
    pub wm_class: String,
    pub strategy: Strategy,
    /// Custom data folder; `None` means `~/.var/app/<app-id>/instances/<slug>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub badge: Option<Badge>,
    #[serde(default)]
    pub autostart: bool,
    /// A `--filesystem` override Multipak added for this instance (removed on delete).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_override: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created: OffsetDateTime,
}

impl Instance {
    pub fn key(&self) -> InstanceKey {
        InstanceKey::new(&self.app_id, &self.slug)
    }

    /// The folder holding this instance's data.
    pub fn resolved_data_dir(&self, paths: &Paths) -> PathBuf {
        self.data_dir.clone().unwrap_or_else(|| paths.default_instance_dir(&self.app_id, &self.slug))
    }

    /// `"<App Name> (<Instance Name>)"`
    pub fn display_name(&self) -> String {
        format!("{} ({})", self.app_name, self.name)
    }

    /// Turns a saved instance back into an editable draft.
    pub fn to_draft(&self) -> InstanceDraft {
        InstanceDraft {
            app_id: self.app_id.clone(),
            app_name: self.app_name.clone(),
            name: self.name.clone(),
            slug: self.slug.clone(),
            wm_class: self.wm_class.clone(),
            strategy: self.strategy,
            data_dir: self.data_dir.clone(),
            extra_args: self.extra_args.clone(),
            env: self.env.clone(),
            badge: self.badge.clone(),
            autostart: self.autostart,
        }
    }
}

/// User input for creating or editing an instance, before validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceDraft {
    pub app_id: String,
    pub app_name: String,
    pub name: String,
    pub slug: String,
    pub wm_class: String,
    pub strategy: Strategy,
    pub data_dir: Option<PathBuf>,
    pub extra_args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub badge: Option<Badge>,
    pub autostart: bool,
}

/// The form field a validation error belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Field {
    AppId,
    Name,
    Slug,
    WmClass,
    DataDir,
    Args,
    Env,
    Badge,
}

impl Field {
    pub fn label(self) -> &'static str {
        match self {
            Self::AppId => "app ID",
            Self::Name => "name",
            Self::Slug => "short name",
            Self::WmClass => "window class",
            Self::DataDir => "data folder",
            Self::Args => "extra arguments",
            Self::Env => "environment variables",
            Self::Badge => "icon badge",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldError {
    pub field: Field,
    pub message: String,
}

/// What validation needs to know about the rest of the setup.
#[derive(Debug, Clone, Copy)]
pub struct DraftContext<'a> {
    pub paths: &'a Paths,
    pub existing: &'a [Instance],
    /// The instance being edited (excluded from uniqueness checks).
    pub editing: Option<&'a InstanceKey>,
}

/// A draft that passed validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedDraft {
    pub instance: Instance,
    pub data_dir: PathBuf,
    /// The `--filesystem` value that must be granted, if the data folder is outside the
    /// app's own `~/.var/app/<app-id>` folder.
    pub needs_override: Option<String>,
    pub warnings: Vec<String>,
}

impl InstanceDraft {
    /// A new draft with generated slug and window class.
    pub fn new(app_id: &str, app_name: &str, name: &str, strategy: Strategy, existing: &[Instance]) -> Self {
        let slug = unique_slug(&slugify(name), |s| existing.iter().any(|i| i.app_id == app_id && i.slug == s));
        Self {
            app_id: app_id.to_string(),
            app_name: app_name.to_string(),
            name: name.trim().to_string(),
            wm_class: default_wm_class(app_id, &slug),
            slug,
            strategy,
            data_dir: None,
            extra_args: Vec::new(),
            env: BTreeMap::new(),
            badge: None,
            autostart: false,
        }
    }

    /// Validates every field and returns all problems at once.
    pub fn validate(&self, ctx: DraftContext<'_>) -> std::result::Result<ValidatedDraft, Vec<FieldError>> {
        let mut errors = Vec::new();
        let mut warnings = Vec::new();
        let mut err = |field, e: Error| {
            errors.push(FieldError {
                field,
                message: match e {
                    Error::Invalid { reason, .. } => reason,
                    other => other.to_string(),
                },
            })
        };
        let others: Vec<&Instance> = ctx.existing.iter().filter(|i| Some(&i.key()) != ctx.editing).collect();

        if let Err(e) = validate_app_id(&self.app_id) {
            err(Field::AppId, e);
        }
        let name = self.name.trim();
        if let Err(e) = validate_name(name) {
            err(Field::Name, e);
        }
        match validate_slug(&self.slug) {
            Err(e) => err(Field::Slug, e),
            Ok(()) => {
                if others.iter().any(|i| i.app_id == self.app_id && i.slug == self.slug) {
                    err(
                        Field::Slug,
                        Error::invalid("short name", "another instance of this app already uses this short name"),
                    );
                }
            }
        }
        match validate_wm_class(&self.wm_class) {
            Err(e) => err(Field::WmClass, e),
            Ok(()) => {
                if others.iter().any(|i| i.wm_class == self.wm_class) {
                    err(
                        Field::WmClass,
                        Error::invalid("window class", "another instance already uses this window class"),
                    );
                }
            }
        }
        for arg in &self.extra_args {
            if let Err(e) = validate_arg(arg) {
                err(Field::Args, e);
                break;
            }
        }
        for flag in strategy_owned_args(self.strategy) {
            if self.extra_args.iter().any(|a| a == flag || a.starts_with(&format!("{flag}="))) {
                warnings.push(format!(
                    "`{flag}` is already set by the {} strategy; passing it again may break isolation.",
                    self.strategy.label()
                ));
            }
        }
        for (k, v) in &self.env {
            if let Err(e) = validate_env(k, v) {
                err(Field::Env, e);
                break;
            }
            if strategy_owned_env(self.strategy).contains(&k.as_str()) {
                warnings.push(format!(
                    "{k} is set by the {} strategy; overriding it may make this instance share data with others.",
                    self.strategy.label()
                ));
            }
        }
        if let Some(badge) = &self.badge {
            if let Err(e) = badge.rgb() {
                err(Field::Badge, e);
            }
            if let Some(letter) = &badge.letter {
                let n = letter.chars().count();
                if n != 1 || letter.chars().any(|c| c.is_whitespace() || c.is_control()) {
                    err(Field::Badge, Error::invalid("badge", "the badge letter must be a single visible character"));
                }
            }
        }

        let data_dir = match &self.data_dir {
            Some(raw) => match normalize_dir(raw) {
                Ok(dir) => dir,
                Err(e) => {
                    err(Field::DataDir, e);
                    PathBuf::new()
                }
            },
            None => ctx.paths.default_instance_dir(&self.app_id, &self.slug),
        };
        let mut needs_override = None;
        if !data_dir.as_os_str().is_empty() {
            if let Err(e) = check_data_dir_location(ctx.paths, &self.app_id, &data_dir) {
                err(Field::DataDir, e);
            } else if let Some(other) =
                others.iter().find(|i| paths_overlap(&i.resolved_data_dir(ctx.paths), &data_dir))
            {
                err(
                    Field::DataDir,
                    Error::invalid(
                        "data folder",
                        format!("this folder overlaps the data folder of {}", other.display_name()),
                    ),
                );
            } else if !crate::flatpak::is_inside_app_dir(ctx.paths, &self.app_id, &data_dir) {
                needs_override = data_dir.to_str().map(str::to_string);
            }
        }

        if !errors.is_empty() {
            return Err(errors);
        }
        let app_name =
            if self.app_name.trim().is_empty() { self.app_id.clone() } else { self.app_name.trim().to_string() };
        let custom = self.data_dir.as_ref().map(|_| data_dir.clone());
        Ok(ValidatedDraft {
            instance: Instance {
                app_id: self.app_id.clone(),
                app_name,
                name: name.to_string(),
                slug: self.slug.clone(),
                wm_class: self.wm_class.clone(),
                strategy: self.strategy,
                data_dir: custom,
                extra_args: self.extra_args.clone(),
                env: self.env.clone(),
                badge: self.badge.clone(),
                autostart: self.autostart,
                managed_override: None,
                created: OffsetDateTime::now_utc(),
            },
            data_dir,
            needs_override,
            warnings,
        })
    }
}

/// Arguments each strategy adds itself.
pub fn strategy_owned_args(strategy: Strategy) -> &'static [&'static str] {
    match strategy {
        Strategy::Electron => &["--class", "--user-data-dir"],
        Strategy::Chromium => &["--user-data-dir", "--class"],
        Strategy::Firefox => &["-profile", "--profile", "--new-instance", "--name", "--class", "-P"],
        Strategy::Generic => &[],
    }
}

/// Environment variables each strategy sets itself.
pub fn strategy_owned_env(strategy: Strategy) -> &'static [&'static str] {
    match strategy {
        Strategy::Electron => &["XDG_CONFIG_HOME"],
        Strategy::Generic => &["XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME"],
        Strategy::Chromium | Strategy::Firefox => &[],
    }
}

/// Flatpak application IDs: at least two dot-separated parts of `[A-Za-z0-9_-]`,
/// each not starting with a digit, at most 255 characters.
pub fn validate_app_id(id: &str) -> Result<()> {
    let bad = |r: &str| Err(Error::invalid("app ID", format!("`{id}` {r}")));
    if id.is_empty() || id.len() > 255 {
        return bad("must be between 1 and 255 characters");
    }
    let parts: Vec<&str> = id.split('.').collect();
    if parts.len() < 2 {
        return bad("must contain at least one dot");
    }
    for p in parts {
        if p.is_empty() {
            return bad("has an empty part");
        }
        if p.starts_with(|c: char| c.is_ascii_digit()) {
            return bad("has a part starting with a digit");
        }
        if !p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return bad("contains characters other than letters, digits, _ and -");
        }
    }
    Ok(())
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        return Err(Error::invalid("name", "the name cannot be empty"));
    }
    if name.chars().count() > MAX_NAME_LEN {
        return Err(Error::invalid("name", format!("the name can be at most {MAX_NAME_LEN} characters")));
    }
    if name.chars().any(char::is_control) {
        return Err(Error::invalid("name", "the name cannot contain line breaks or control characters"));
    }
    Ok(())
}

/// Slugs: lowercase letters, digits and single dashes, 1 to 48 characters.
pub fn validate_slug(slug: &str) -> Result<()> {
    let ok = !slug.is_empty()
        && slug.len() <= MAX_SLUG_LEN
        && slug.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !slug.starts_with('-')
        && !slug.ends_with('-')
        && !slug.contains("--");
    if ok {
        Ok(())
    } else {
        Err(Error::invalid(
            "short name",
            format!("use 1–{MAX_SLUG_LEN} lowercase letters, digits and single dashes (not at the start or end)"),
        ))
    }
}

/// Window classes: letters, digits, `.`, `_` and `-`, starting with a letter or digit.
pub fn validate_wm_class(class: &str) -> Result<()> {
    let ok = !class.is_empty()
        && class.len() <= MAX_WM_CLASS_LEN
        && class.starts_with(|c: char| c.is_ascii_alphanumeric())
        && class.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    if ok {
        Ok(())
    } else {
        Err(Error::invalid(
            "window class",
            format!(
                "use 1–{MAX_WM_CLASS_LEN} letters, digits, dots, dashes and underscores, starting with a letter or digit"
            ),
        ))
    }
}

pub fn validate_arg(arg: &str) -> Result<()> {
    if arg.contains('\0') || arg.contains('\n') || arg.contains('\r') {
        return Err(Error::invalid("extra arguments", "arguments cannot contain line breaks or NUL characters"));
    }
    Ok(())
}

pub fn validate_env(key: &str, value: &str) -> Result<()> {
    let key_ok = key.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !key_ok {
        return Err(Error::invalid("environment variables", format!("`{key}` is not a valid variable name")));
    }
    if key == MARKER_ENV {
        return Err(Error::invalid("environment variables", format!("{MARKER_ENV} is reserved for Multipak")));
    }
    if value.chars().any(|c| c == '\0' || c == '\n' || c == '\r') {
        return Err(Error::invalid("environment variables", format!("the value of {key} cannot contain line breaks")));
    }
    Ok(())
}

/// Makes a custom folder path canonical without touching the filesystem.
pub fn normalize_dir(raw: &Path) -> Result<PathBuf> {
    let Some(text) = raw.to_str() else {
        return Err(Error::invalid("data folder", "the path must be valid UTF-8"));
    };
    if !raw.is_absolute() {
        return Err(Error::invalid("data folder", "use an absolute path (starting with /)"));
    }
    if text.chars().any(char::is_control) || text.contains(';') {
        return Err(Error::invalid("data folder", "the path cannot contain `;` or control characters"));
    }
    let mut out = PathBuf::from("/");
    for c in raw.components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::ParentDir => {
                return Err(Error::invalid("data folder", "the path cannot contain `..`"));
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    Ok(out)
}

/// Rejects data folders that are system or XDG base folders (or their parents).
pub fn check_data_dir_location(paths: &Paths, app_id: &str, dir: &Path) -> Result<()> {
    let mut protected = paths.protected_dirs();
    protected.push(paths.app_var_dir(app_id));
    protected.push(paths.instances_root(app_id));
    for p in protected {
        if p.starts_with(dir) {
            return Err(Error::invalid(
                "data folder",
                format!("{} is a system or shared folder; pick a dedicated folder for this instance", dir.display()),
            ));
        }
    }
    for system in ["/usr", "/etc", "/proc", "/sys", "/dev", "/run", "/boot", "/bin", "/sbin", "/lib", "/lib64", "/app"]
    {
        if dir.starts_with(system) {
            return Err(Error::invalid("data folder", format!("{system} cannot hold instance data")));
        }
    }
    Ok(())
}

/// `"<name> (copy)"`, shortened so it stays a valid name.
pub fn copy_name(name: &str) -> String {
    let suffix = " (copy)";
    let max = MAX_NAME_LEN - suffix.chars().count();
    let stem: String = name.trim().chars().take(max).collect();
    format!("{}{suffix}", stem.trim_end())
}

/// Turns a display name into a slug. Non-ASCII names fall back to `instance`.
pub fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            if dash && !out.is_empty() {
                out.push('-');
            }
            dash = false;
            out.push(c.to_ascii_lowercase());
        } else {
            dash = true;
        }
        if out.len() >= MAX_SLUG_LEN {
            break;
        }
    }
    out.truncate(MAX_SLUG_LEN);
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() { "instance".to_string() } else { out }
}

/// Appends `-2`, `-3`, … until `taken` returns false.
pub fn unique_slug(base: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(base) {
        return base.to_string();
    }
    (2..)
        .map(|n| {
            let suffix = format!("-{n}");
            let mut stem = base.to_string();
            stem.truncate(MAX_SLUG_LEN - suffix.len());
            format!("{}{suffix}", stem.trim_end_matches('-'))
        })
        .find(|s| !taken(s))
        .expect("an unused slug always exists")
}

/// `multipak-<last part of app id>-<slug>`, e.g. `multipak-slack-work`.
pub fn default_wm_class(app_id: &str, slug: &str) -> String {
    let last: String = app_id
        .rsplit('.')
        .next()
        .unwrap_or(app_id)
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let mut class = format!("multipak-{}-{slug}", last.trim_matches('-'));
    class.truncate(MAX_WM_CLASS_LEN);
    class
}

/// Parses `#rrggbb` (or `rrggbb`).
pub fn parse_hex_color(s: &str) -> Result<[u8; 3]> {
    let hex = s.trim().trim_start_matches('#');
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Error::invalid("badge", format!("`{s}` is not a color like #3584e4")));
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).expect("validated hex");
    Ok([byte(0), byte(2), byte(4)])
}

pub fn format_hex_color(rgb: [u8; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2])
}

/// Parses `KEY=VALUE`.
pub fn parse_env_pair(s: &str) -> Result<(String, String)> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| Error::invalid("environment variables", format!("`{s}` is not in KEY=VALUE form")))?;
    validate_env(k.trim(), v)?;
    Ok((k.trim().to_string(), v.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> Paths {
        Paths::for_home("/home/u")
    }

    fn draft(name: &str) -> InstanceDraft {
        InstanceDraft::new("com.slack.Slack", "Slack", name, Strategy::Electron, &[])
    }

    fn validated(d: &InstanceDraft, existing: &[Instance]) -> std::result::Result<ValidatedDraft, Vec<FieldError>> {
        d.validate(DraftContext { paths: &paths(), existing, editing: None })
    }

    #[test]
    fn slugify_examples() {
        assert_eq!(slugify("Work"), "work");
        assert_eq!(slugify("  My  Client: ACME!! "), "my-client-acme");
        assert_eq!(slugify("العمل"), "instance");
        assert_eq!(slugify("Ünïcödé 2"), "n-c-d-2");
        assert_eq!(slugify(&"a".repeat(100)).len(), MAX_SLUG_LEN);
        assert!(validate_slug(&slugify("--x--y--")).is_ok());
    }

    #[test]
    fn unique_slug_appends_numbers() {
        let taken = ["work", "work-2"];
        assert_eq!(unique_slug("work", |s| taken.contains(&s)), "work-3");
        assert_eq!(unique_slug("home", |s| taken.contains(&s)), "home");
        let long = "a".repeat(MAX_SLUG_LEN);
        let s = unique_slug(&long, |s| s == long);
        assert_eq!(s.len(), MAX_SLUG_LEN);
        assert!(s.ends_with("-2"));
    }

    #[test]
    fn slug_validation() {
        for ok in ["a", "work", "client-1", "x2"] {
            assert!(validate_slug(ok).is_ok(), "{ok}");
        }
        for bad in ["", "Work", "-a", "a-", "a--b", "a b", "a_b", "é", &"a".repeat(49)] {
            assert!(validate_slug(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn wm_class_validation() {
        for ok in ["multipak-slack-work", "Slack.Work_2", "a"] {
            assert!(validate_wm_class(ok).is_ok(), "{ok}");
        }
        for bad in ["", "-a", ".a", "a b", "a/b", "a;b", &"a".repeat(129)] {
            assert!(validate_wm_class(bad).is_err(), "{bad}");
        }
        assert_eq!(default_wm_class("com.slack.Slack", "work"), "multipak-slack-work");
        assert_eq!(default_wm_class("io.gitlab.librewolf-community", "a"), "multipak-librewolf-community-a");
    }

    #[test]
    fn app_id_validation() {
        assert!(validate_app_id("com.slack.Slack").is_ok());
        assert!(validate_app_id("io.gitlab.librewolf-community").is_ok());
        for bad in ["slack", "com..x", "com.1x", "com.x/../y", "com.x y"] {
            assert!(validate_app_id(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn instance_key_parsing() {
        let k = InstanceKey::parse("com.slack.Slack/work").unwrap();
        assert_eq!(k, InstanceKey::new("com.slack.Slack", "work"));
        assert_eq!(k.to_string(), "com.slack.Slack/work");
        assert!(InstanceKey::parse("work").is_none());
        assert!(InstanceKey::parse("com.slack.Slack/Work").is_none());
    }

    #[test]
    fn valid_draft_uses_default_dir() {
        let v = validated(&draft("Work"), &[]).unwrap();
        assert_eq!(v.data_dir, PathBuf::from("/home/u/.var/app/com.slack.Slack/instances/work"));
        assert_eq!(v.instance.data_dir, None);
        assert_eq!(v.needs_override, None);
        assert_eq!(v.instance.wm_class, "multipak-slack-work");
    }

    #[test]
    fn custom_dir_outside_app_folder_needs_override() {
        let mut d = draft("Work");
        d.data_dir = Some(PathBuf::from("/home/u/Clients/acme/./slack/"));
        let v = validated(&d, &[]).unwrap();
        assert_eq!(v.data_dir, PathBuf::from("/home/u/Clients/acme/slack"));
        assert_eq!(v.needs_override.as_deref(), Some("/home/u/Clients/acme/slack"));

        d.data_dir = Some(PathBuf::from("/home/u/.var/app/com.slack.Slack/custom"));
        assert_eq!(validated(&d, &[]).unwrap().needs_override, None);
    }

    #[test]
    fn dangerous_dirs_are_rejected() {
        for bad in [
            "/",
            "/home/u",
            "/home",
            "/home/u/.var/app",
            "/home/u/.config",
            "/usr/share/x",
            "relative",
            "/home/u/../x",
            "/home/u/a;b",
        ] {
            let mut d = draft("Work");
            d.data_dir = Some(PathBuf::from(bad));
            let errs = validated(&d, &[]).unwrap_err();
            assert!(errs.iter().any(|e| e.field == Field::DataDir), "{bad} accepted");
        }
    }

    #[test]
    fn duplicates_and_overlaps_are_rejected() {
        let first = validated(&draft("Work"), &[]).unwrap().instance;
        let existing = vec![first.clone()];
        let mut d = draft("Work");
        d.slug = "work".into();
        let errs = validated(&d, &existing).unwrap_err();
        assert!(errs.iter().any(|e| e.field == Field::Slug));
        assert!(errs.iter().any(|e| e.field == Field::WmClass));

        let mut d = draft("Other");
        d.data_dir = Some(PathBuf::from("/home/u/.var/app/com.slack.Slack/instances/work/nested"));
        let errs = validated(&d, &existing).unwrap_err();
        assert!(errs.iter().any(|e| e.field == Field::DataDir));

        // Editing the existing instance does not conflict with itself.
        let key = first.key();
        let ok = first.to_draft().validate(DraftContext { paths: &paths(), existing: &existing, editing: Some(&key) });
        assert!(ok.is_ok());
    }

    #[test]
    fn new_draft_avoids_taken_slugs() {
        let first = validated(&draft("Work"), &[]).unwrap().instance;
        let d = InstanceDraft::new("com.slack.Slack", "Slack", "Work", Strategy::Electron, &[first]);
        assert_eq!(d.slug, "work-2");
        assert_eq!(d.wm_class, "multipak-slack-work-2");
    }

    #[test]
    fn env_args_and_badge_validation() {
        let mut d = draft("Work");
        d.env.insert("1BAD".into(), "x".into());
        assert!(validated(&d, &[]).unwrap_err().iter().any(|e| e.field == Field::Env));
        d.env.clear();
        d.env.insert(MARKER_ENV.into(), "x".into());
        assert!(validated(&d, &[]).is_err());
        d.env.clear();
        d.env.insert("XDG_CONFIG_HOME".into(), "/tmp".into());
        assert_eq!(validated(&d, &[]).unwrap().warnings.len(), 1);
        d.env.clear();
        d.extra_args = vec!["--a\nb".into()];
        assert!(validated(&d, &[]).unwrap_err().iter().any(|e| e.field == Field::Args));
        d.extra_args = vec!["--class=x".into()];
        assert_eq!(validated(&d, &[]).unwrap().warnings.len(), 1);
        d.extra_args.clear();
        d.badge = Some(Badge { color: "red".into(), letter: Some("ab".into()) });
        assert_eq!(validated(&d, &[]).unwrap_err().iter().filter(|e| e.field == Field::Badge).count(), 2);
        d.badge = Some(Badge { color: "#3584E4".into(), letter: Some("W".into()) });
        assert!(validated(&d, &[]).is_ok());
    }

    #[test]
    fn copy_names_stay_valid() {
        assert_eq!(copy_name("Work"), "Work (copy)");
        let long = copy_name(&"x".repeat(80));
        assert!(validate_name(&long).is_ok());
        assert!(long.ends_with(" (copy)"));
    }

    #[test]
    fn colors_and_env_pairs() {
        assert_eq!(parse_hex_color("#3584e4").unwrap(), [0x35, 0x84, 0xe4]);
        assert_eq!(format_hex_color([1, 2, 255]), "#0102ff");
        assert!(parse_hex_color("#12345").is_err());
        assert_eq!(parse_env_pair("A=b=c").unwrap(), ("A".into(), "b=c".into()));
        assert!(parse_env_pair("novalue").is_err());
    }
}
