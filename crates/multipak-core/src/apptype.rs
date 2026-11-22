//! Launch strategies and how Multipak picks one for an app.
//!
//! Detection order:
//! 1. the built-in table (`data/app_types.toml`) merged with the user's
//!    `~/.config/multipak/app_types.toml`;
//! 2. the app's base app from its Flatpak metadata (e.g. `org.electronjs.Electron2.BaseApp`);
//! 3. marker files inside the installed app (`*.asar`, `omni.ja`, `chrome-sandbox`, …);
//! 4. the generic strategy.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::flatpak::AppMetadata;
use crate::keyfile::KeyFile;
use crate::paths::Paths;

const BUILTIN_TABLE: &str = include_str!("../data/app_types.toml");

/// How an instance is given its own data folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Strategy {
    /// `--env=XDG_CONFIG_HOME=<dir>` and `--class=<wmclass>`.
    Electron,
    /// `--user-data-dir=<dir>` and `--class=<wmclass>`.
    Chromium,
    /// A dedicated profile: `-profile <dir> --new-instance`.
    Firefox,
    /// `XDG_CONFIG_HOME`, `XDG_DATA_HOME` and `XDG_CACHE_HOME` inside the data folder.
    Generic,
}

impl Strategy {
    pub const ALL: [Strategy; 4] = [Self::Electron, Self::Chromium, Self::Firefox, Self::Generic];

    pub fn label(self) -> &'static str {
        match self {
            Self::Electron => "Electron app",
            Self::Chromium => "Chromium-based browser",
            Self::Firefox => "Firefox-based browser",
            Self::Generic => "Generic (XDG folders)",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Electron => {
                "Sets XDG_CONFIG_HOME to the data folder and passes --class. Works for Slack, Discord, Obsidian, Signal and most Electron apps."
            }
            Self::Chromium => {
                "Passes --user-data-dir and --class. Works for Chrome, Chromium, Brave, Edge and Vivaldi."
            }
            Self::Firefox => {
                "Creates a dedicated profile in the data folder and starts it with -profile --new-instance."
            }
            Self::Generic => {
                "Points XDG_CONFIG_HOME, XDG_DATA_HOME and XDG_CACHE_HOME at subfolders of the data folder."
            }
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Electron => "electron",
            Self::Chromium => "chromium",
            Self::Firefox => "firefox",
            Self::Generic => "generic",
        }
    }
}

impl fmt::Display for Strategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

impl std::str::FromStr for Strategy {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        Self::ALL.into_iter().find(|k| k.id().eq_ignore_ascii_case(s.trim())).ok_or_else(|| {
            Error::invalid("strategy", format!("`{s}` is not one of electron, chromium, firefox, generic"))
        })
    }
}

/// One entry of the app-type table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownApp {
    pub id: String,
    pub kind: Strategy,
    #[serde(default)]
    pub single_instance: bool,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TableFile {
    #[serde(default, rename = "app")]
    apps: Vec<KnownApp>,
}

/// The merged table of known apps.
#[derive(Debug, Clone, Default)]
pub struct AppTypeTable {
    apps: HashMap<String, KnownApp>,
}

impl AppTypeTable {
    /// Parses a table in the `app_types.toml` format.
    pub fn parse(text: &str, what: &str) -> Result<Self> {
        let file: TableFile =
            toml::from_str(text).map_err(|e| Error::Parse { what: what.to_string(), reason: e.to_string() })?;
        Ok(Self { apps: file.apps.into_iter().map(|a| (a.id.clone(), a)).collect() })
    }

    /// The table shipped with Multipak.
    pub fn builtin() -> Self {
        Self::parse(BUILTIN_TABLE, "built-in app table").expect("built-in app table is valid TOML")
    }

    /// The built-in table plus the user's additions (which win on conflicts).
    /// A broken user file is reported, not silently ignored.
    pub fn load(paths: &Paths) -> Result<Self> {
        let mut table = Self::builtin();
        let file = paths.user_app_types_file();
        match std::fs::read_to_string(&file) {
            Ok(text) => table.merge(Self::parse(&text, &file.display().to_string())?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(Error::Io { context: format!("could not read {}", file.display()), source: e });
            }
        }
        Ok(table)
    }

    pub fn merge(&mut self, other: Self) {
        self.apps.extend(other.apps);
    }

    pub fn get(&self, app_id: &str) -> Option<&KnownApp> {
        self.apps.get(app_id)
    }

    pub fn len(&self) -> usize {
        self.apps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.apps.is_empty()
    }

    /// All entries sorted by kind then ID (for documentation and the UI).
    pub fn entries(&self) -> Vec<&KnownApp> {
        let mut v: Vec<_> = self.apps.values().collect();
        v.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.id.cmp(&b.id)));
        v
    }
}

/// How confident Multipak is that a second copy of the app will really be separate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SingleInstanceRisk {
    Unlikely,
    /// The app might forward a second launch to the running copy.
    Possible(String),
    /// The app is known (or strongly suspected) to allow only one running copy.
    Likely(String),
}

impl SingleInstanceRisk {
    pub fn warning(&self) -> Option<&str> {
        match self {
            Self::Unlikely => None,
            Self::Possible(s) | Self::Likely(s) => Some(s),
        }
    }
}

/// Where a detected strategy came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DetectionSource {
    Table,
    BaseApp(String),
    Files(String),
    Fallback,
}

impl fmt::Display for DetectionSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Table => f.write_str("listed in the app table"),
            Self::BaseApp(b) => write!(f, "built on {b}"),
            Self::Files(m) => write!(f, "contains {m}"),
            Self::Fallback => f.write_str("no specific type detected"),
        }
    }
}

/// Result of [`detect`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Detection {
    pub strategy: Strategy,
    pub source: DetectionSource,
    pub single_instance: SingleInstanceRisk,
    pub note: Option<String>,
}

/// Inputs available for detecting an app's type. All are optional: detection degrades
/// gracefully when Flatpak metadata or files cannot be read.
#[derive(Debug, Default, Clone, Copy)]
pub struct DetectInput<'a> {
    pub metadata: Option<&'a AppMetadata>,
    /// The app's installed `files/` folder.
    pub files_dir: Option<&'a Path>,
    /// The app's exported `.desktop` file.
    pub desktop: Option<&'a KeyFile>,
}

/// Picks a strategy for `app_id` and estimates the single-instance risk.
pub fn detect(table: &AppTypeTable, app_id: &str, input: DetectInput<'_>) -> Detection {
    let (strategy, source, note, listed_single) = if let Some(known) = table.get(app_id) {
        (known.kind, DetectionSource::Table, known.note.clone(), known.single_instance)
    } else if let Some((kind, base)) = input.metadata.and_then(strategy_from_base) {
        (kind, DetectionSource::BaseApp(base), None, false)
    } else if let Some((kind, marker)) = input.files_dir.and_then(strategy_from_files) {
        (kind, DetectionSource::Files(marker), None, false)
    } else {
        (Strategy::Generic, DetectionSource::Fallback, None, false)
    };
    let single_instance = single_instance_risk(strategy, listed_single, input);
    Detection { strategy, source, single_instance, note }
}

fn base_name(base: &str) -> &str {
    // "app/org.electronjs.Electron2.BaseApp/x86_64/25.08" -> "org.electronjs.Electron2.BaseApp"
    base.split('/').find(|p| p.contains('.')).unwrap_or(base)
}

fn strategy_from_base(md: &AppMetadata) -> Option<(Strategy, String)> {
    let name = base_name(md.base.as_deref()?).to_string();
    let lower = name.to_lowercase();
    let kind = if lower.starts_with("org.electronjs.") {
        Strategy::Electron
    } else if lower.starts_with("org.chromium.chromium.baseapp") {
        Strategy::Chromium
    } else if lower.starts_with("org.mozilla.firefox.baseapp") {
        Strategy::Firefox
    } else {
        return None;
    };
    Some((kind, name))
}

/// Looks for well-known files inside the app. The search is bounded so a huge app never
/// stalls detection.
fn strategy_from_files(files: &Path) -> Option<(Strategy, String)> {
    let mut has_asar = None;
    let mut has_omni = None;
    let mut has_app_ini = false;
    let mut has_chrome = None;
    let mut has_cef = false;
    for entry in
        walkdir::WalkDir::new(files).max_depth(5).follow_links(false).into_iter().filter_map(|e| e.ok()).take(30_000)
    {
        let name = entry.file_name().to_string_lossy();
        if name.ends_with(".asar") && has_asar.is_none() {
            has_asar = Some(name.into_owned());
        } else if name == "omni.ja" {
            has_omni = Some(name.into_owned());
        } else if name == "application.ini" {
            has_app_ini = true;
        } else if (name == "chrome-sandbox" || name == "chrome_crashpad_handler") && has_chrome.is_none() {
            has_chrome = Some(name.into_owned());
        } else if name == "libcef.so" {
            // The Chromium Embedded Framework (OBS Studio, Spotify, …) ships Chromium's
            // helper binaries, but the app itself is not a browser.
            has_cef = true;
        }
    }
    if let Some(m) = has_asar {
        Some((Strategy::Electron, m))
    } else if let (Some(m), true) = (has_omni, has_app_ini) {
        Some((Strategy::Firefox, m))
    } else if has_cef {
        None
    } else {
        has_chrome.map(|m| (Strategy::Chromium, m))
    }
}

fn single_instance_risk(strategy: Strategy, listed: bool, input: DetectInput<'_>) -> SingleInstanceRisk {
    if listed {
        return SingleInstanceRisk::Likely(
            "This app is known to allow only one running copy. A second launch will probably just open a window in the first one.".into(),
        );
    }
    if strategy != Strategy::Generic {
        // Electron, Chromium and Firefox all key their single-instance lock on the data
        // folder, which is exactly what Multipak changes.
        return SingleInstanceRisk::Unlikely;
    }
    if input.desktop.and_then(|d| d.get_bool("Desktop Entry", "DBusActivatable")) == Some(true) {
        return SingleInstanceRisk::Likely(
            "This app is D-Bus activatable, so it enforces a single running copy over D-Bus. A second launch will probably open a window in the first one.".into(),
        );
    }
    let runtime = input.metadata.and_then(|m| m.runtime.as_deref()).unwrap_or("");
    if runtime.starts_with("org.gnome.Platform") {
        return SingleInstanceRisk::Possible(
            "GNOME apps usually use GApplication, which allows only one running copy per user. If the app is already open, a second copy may just open a new window in the first one.".into(),
        );
    }
    if runtime.starts_with("org.kde.Platform") {
        return SingleInstanceRisk::Possible(
            "Many KDE apps register a unique D-Bus name. If the app is already open, a second copy may just activate the first one.".into(),
        );
    }
    SingleInstanceRisk::Unlikely
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_table_is_valid_and_has_entries() {
        let t = AppTypeTable::builtin();
        assert!(t.len() > 20);
        assert_eq!(t.get("com.slack.Slack").unwrap().kind, Strategy::Electron);
        assert_eq!(t.get("com.google.Chrome").unwrap().kind, Strategy::Chromium);
        assert_eq!(t.get("org.mozilla.firefox").unwrap().kind, Strategy::Firefox);
    }

    #[test]
    fn user_table_overrides_builtin() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::for_home(dir.path());
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        std::fs::write(
            paths.user_app_types_file(),
            "[[app]]\nid = \"com.slack.Slack\"\nkind = \"generic\"\nsingle_instance = true\n\n[[app]]\nid = \"org.example.New\"\nkind = \"chromium\"\n",
        )
        .unwrap();
        let t = AppTypeTable::load(&paths).unwrap();
        assert_eq!(t.get("com.slack.Slack").unwrap().kind, Strategy::Generic);
        assert_eq!(t.get("org.example.New").unwrap().kind, Strategy::Chromium);
        let d = detect(&t, "com.slack.Slack", DetectInput::default());
        assert!(matches!(d.single_instance, SingleInstanceRisk::Likely(_)));
    }

    #[test]
    fn broken_user_table_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::for_home(dir.path());
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        std::fs::write(paths.user_app_types_file(), "[[app]]\nid = 3\n").unwrap();
        assert!(matches!(AppTypeTable::load(&paths), Err(Error::Parse { .. })));
    }

    #[test]
    fn detects_from_base_app() {
        let t = AppTypeTable::default();
        let md = AppMetadata {
            base: Some("app/org.electronjs.Electron2.BaseApp/x86_64/25.08".into()),
            ..Default::default()
        };
        let d = detect(&t, "com.unknown.App", DetectInput { metadata: Some(&md), ..Default::default() });
        assert_eq!(d.strategy, Strategy::Electron);
        assert_eq!(d.source, DetectionSource::BaseApp("org.electronjs.Electron2.BaseApp".into()));
        let md =
            AppMetadata { base: Some("app/org.chromium.Chromium.BaseApp/x86_64/24.08".into()), ..Default::default() };
        let d = detect(&t, "com.unknown.Browser", DetectInput { metadata: Some(&md), ..Default::default() });
        assert_eq!(d.strategy, Strategy::Chromium);
    }

    #[test]
    fn detects_from_files() {
        let t = AppTypeTable::default();
        let dir = tempfile::tempdir().unwrap();
        let ff = dir.path().join("ff");
        std::fs::create_dir_all(ff.join("lib/browser")).unwrap();
        std::fs::write(ff.join("lib/omni.ja"), "").unwrap();
        std::fs::write(ff.join("lib/application.ini"), "").unwrap();
        let d = detect(&t, "x.y.Z", DetectInput { files_dir: Some(&ff), ..Default::default() });
        assert_eq!(d.strategy, Strategy::Firefox);

        let el = dir.path().join("el");
        std::fs::create_dir_all(el.join("extra/resources")).unwrap();
        std::fs::write(el.join("extra/resources/app.asar"), "").unwrap();
        std::fs::write(el.join("extra/chrome-sandbox"), "").unwrap();
        let d = detect(&t, "x.y.Z", DetectInput { files_dir: Some(&el), ..Default::default() });
        assert_eq!(d.strategy, Strategy::Electron);

        let cr = dir.path().join("cr");
        std::fs::create_dir_all(&cr).unwrap();
        std::fs::write(cr.join("chrome-sandbox"), "").unwrap();
        let d = detect(&t, "x.y.Z", DetectInput { files_dir: Some(&cr), ..Default::default() });
        assert_eq!(d.strategy, Strategy::Chromium);

        // Apps embedding CEF are not browsers.
        std::fs::write(cr.join("libcef.so"), "").unwrap();
        let d = detect(&t, "x.y.Z", DetectInput { files_dir: Some(&cr), ..Default::default() });
        assert_eq!(d.strategy, Strategy::Generic);
    }

    #[test]
    fn warns_about_single_instance_apps() {
        let t = AppTypeTable::default();
        let md = AppMetadata { runtime: Some("org.gnome.Platform/x86_64/47".into()), ..Default::default() };
        let d = detect(&t, "org.gnome.Foo", DetectInput { metadata: Some(&md), ..Default::default() });
        assert_eq!(d.strategy, Strategy::Generic);
        assert!(matches!(d.single_instance, SingleInstanceRisk::Possible(_)));

        let desktop = KeyFile::parse("[Desktop Entry]\nDBusActivatable=true\n");
        let d = detect(
            &t,
            "org.gnome.Foo",
            DetectInput { metadata: Some(&md), desktop: Some(&desktop), ..Default::default() },
        );
        assert!(matches!(d.single_instance, SingleInstanceRisk::Likely(_)));

        let d = detect(&t, "org.freedesktop.Foo", DetectInput::default());
        assert_eq!(d.single_instance, SingleInstanceRisk::Unlikely);
        assert_eq!(d.source, DetectionSource::Fallback);
    }

    #[test]
    fn strategy_parses() {
        assert_eq!("Chromium".parse::<Strategy>().unwrap(), Strategy::Chromium);
        assert!("chrome".parse::<Strategy>().is_err());
    }
}
