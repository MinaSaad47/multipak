//! Locations of every file and folder Multipak reads or writes.
//!
//! All paths are computed once from the environment (or from an explicit home folder
//! in tests) so the rest of the library never reads environment variables itself.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Resolved XDG and Flatpak locations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub home: PathBuf,
    pub xdg_config_home: PathBuf,
    pub xdg_data_home: PathBuf,
    pub xdg_state_home: PathBuf,
    pub xdg_cache_home: PathBuf,
    pub xdg_data_dirs: Vec<PathBuf>,
    pub flatpak_user_dir: PathBuf,
    pub flatpak_system_dirs: Vec<PathBuf>,
}

impl Paths {
    /// Builds the paths from the real process environment.
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|key| std::env::var_os(key))
    }

    /// Builds the paths for a given home folder, ignoring the environment.
    /// The system Flatpak installation is placed inside `home` so tests never touch
    /// the real `/var/lib/flatpak`.
    pub fn for_home(home: impl Into<PathBuf>) -> Self {
        let home = home.into();
        let data = home.join(".local/share");
        Self {
            xdg_config_home: home.join(".config"),
            xdg_state_home: home.join(".local/state"),
            xdg_cache_home: home.join(".cache"),
            xdg_data_dirs: vec![home.join("system-share")],
            flatpak_user_dir: data.join("flatpak"),
            flatpak_system_dirs: vec![home.join("system-flatpak")],
            xdg_data_home: data,
            home,
        }
    }

    /// Builds the paths using `lookup` to read environment variables.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<OsString>) -> Result<Self> {
        let home = lookup("HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .ok_or_else(|| Error::invalid("HOME", "the HOME environment variable is not set to an absolute path"))?;
        let abs = |key: &str| lookup(key).map(PathBuf::from).filter(|p| p.is_absolute());
        let xdg_data_home = abs("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share"));
        let xdg_data_dirs = lookup("XDG_DATA_DIRS")
            .map(|v| std::env::split_paths(&v).filter(|p| p.is_absolute()).collect::<Vec<_>>())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| vec![PathBuf::from("/usr/local/share"), PathBuf::from("/usr/share")]);
        let mut flatpak_system_dirs = Vec::new();
        if let Some(dir) = abs("FLATPAK_SYSTEM_DIR") {
            flatpak_system_dirs.push(dir);
        }
        let default_system = PathBuf::from("/var/lib/flatpak");
        if !flatpak_system_dirs.contains(&default_system) {
            flatpak_system_dirs.push(default_system);
        }
        Ok(Self {
            xdg_config_home: abs("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config")),
            xdg_state_home: abs("XDG_STATE_HOME").unwrap_or_else(|| home.join(".local/state")),
            xdg_cache_home: abs("XDG_CACHE_HOME").unwrap_or_else(|| home.join(".cache")),
            flatpak_user_dir: abs("FLATPAK_USER_DIR").unwrap_or_else(|| xdg_data_home.join("flatpak")),
            xdg_data_dirs,
            flatpak_system_dirs,
            xdg_data_home,
            home,
        })
    }

    /// `~/.config/multipak`
    pub fn config_dir(&self) -> PathBuf {
        self.xdg_config_home.join("multipak")
    }

    /// `~/.config/multipak/instances.toml`
    pub fn config_file(&self) -> PathBuf {
        self.config_dir().join("instances.toml")
    }

    /// `~/.config/multipak/app_types.toml`: user additions to the app-type table.
    pub fn user_app_types_file(&self) -> PathBuf {
        self.config_dir().join("app_types.toml")
    }

    /// `~/.local/share/applications`
    pub fn applications_dir(&self) -> PathBuf {
        self.xdg_data_home.join("applications")
    }

    /// `~/.config/autostart`
    pub fn autostart_dir(&self) -> PathBuf {
        self.xdg_config_home.join("autostart")
    }

    /// `~/.local/share/icons/multipak`
    pub fn badge_icons_dir(&self) -> PathBuf {
        self.xdg_data_home.join("icons").join("multipak")
    }

    /// `~/.local/state/multipak`
    pub fn state_dir(&self) -> PathBuf {
        self.xdg_state_home.join("multipak")
    }

    /// `~/.local/state/multipak/multipak.log`
    pub fn log_file(&self) -> PathBuf {
        self.state_dir().join("multipak.log")
    }

    /// Output of the most recent launch of one instance.
    pub fn launch_log(&self, app_id: &str, slug: &str) -> PathBuf {
        self.state_dir().join("launch-logs").join(format!("{app_id}-{slug}.log"))
    }

    /// `~/.var/app/<app-id>`: the folder the sandbox can always reach.
    pub fn app_var_dir(&self, app_id: &str) -> PathBuf {
        self.home.join(".var/app").join(app_id)
    }

    /// `~/.var/app/<app-id>/instances`
    pub fn instances_root(&self, app_id: &str) -> PathBuf {
        self.app_var_dir(app_id).join("instances")
    }

    /// `~/.var/app/<app-id>/instances/<slug>`
    pub fn default_instance_dir(&self, app_id: &str, slug: &str) -> PathBuf {
        self.instances_root(app_id).join(slug)
    }

    /// File name of an instance launcher.
    pub fn desktop_file_name(app_id: &str, slug: &str) -> String {
        format!("multipak-{app_id}-{slug}.desktop")
    }

    /// `~/.local/share/applications/multipak-<app-id>-<slug>.desktop`
    pub fn desktop_file(&self, app_id: &str, slug: &str) -> PathBuf {
        self.applications_dir().join(Self::desktop_file_name(app_id, slug))
    }

    /// `~/.config/autostart/multipak-<app-id>-<slug>.desktop`
    pub fn autostart_file(&self, app_id: &str, slug: &str) -> PathBuf {
        self.autostart_dir().join(Self::desktop_file_name(app_id, slug))
    }

    /// `~/.local/share/icons/multipak/<app-id>-<slug>.png`
    pub fn badge_icon(&self, app_id: &str, slug: &str) -> PathBuf {
        self.badge_icons_dir().join(format!("{app_id}-{slug}.png"))
    }

    /// The per-user Flatpak overrides file for one app.
    pub fn overrides_file(&self, app_id: &str) -> PathBuf {
        self.flatpak_user_dir.join("overrides").join(app_id)
    }

    /// `exports/share` folders of every Flatpak installation, user first.
    pub fn flatpak_export_share_dirs(&self) -> Vec<PathBuf> {
        std::iter::once(&self.flatpak_user_dir)
            .chain(self.flatpak_system_dirs.iter())
            .map(|d| d.join("exports/share"))
            .collect()
    }

    /// Folders that may contain icon themes and `.desktop` files, in lookup order.
    pub fn share_dirs(&self) -> Vec<PathBuf> {
        let mut dirs = self.flatpak_export_share_dirs();
        dirs.push(self.xdg_data_home.clone());
        dirs.extend(self.xdg_data_dirs.iter().cloned());
        dirs
    }

    /// The exported `.desktop` file of an installed app, if present.
    pub fn exported_desktop_file(&self, app_id: &str) -> Option<PathBuf> {
        self.flatpak_export_share_dirs()
            .into_iter()
            .map(|d| d.join("applications").join(format!("{app_id}.desktop")))
            .find(|p| p.is_file())
    }

    /// Replaces a leading home folder with `~` (for display and portable exports).
    pub fn tildify(&self, path: &Path) -> String {
        match path.strip_prefix(&self.home) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        }
    }

    /// Expands a leading `~/` to the home folder.
    pub fn expand_tilde(&self, raw: &str) -> PathBuf {
        if raw == "~" {
            self.home.clone()
        } else if let Some(rest) = raw.strip_prefix("~/") {
            self.home.join(rest)
        } else {
            PathBuf::from(raw)
        }
    }

    /// Folders that must never be deleted or used as a whole data folder.
    pub fn protected_dirs(&self) -> Vec<PathBuf> {
        let mut dirs = vec![
            PathBuf::from("/"),
            self.home.clone(),
            self.home.join(".var"),
            self.home.join(".var/app"),
            self.xdg_config_home.clone(),
            self.xdg_data_home.clone(),
            self.xdg_state_home.clone(),
            self.xdg_cache_home.clone(),
            self.flatpak_user_dir.clone(),
        ];
        dirs.extend(self.flatpak_system_dirs.iter().cloned());
        dirs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup(map: HashMap<&'static str, &'static str>) -> impl Fn(&str) -> Option<OsString> {
        move |k| map.get(k).map(OsString::from)
    }

    #[test]
    fn defaults_follow_xdg_spec() {
        let p = Paths::from_lookup(lookup(HashMap::from([("HOME", "/home/u")]))).unwrap();
        assert_eq!(p.config_file(), PathBuf::from("/home/u/.config/multipak/instances.toml"));
        assert_eq!(p.applications_dir(), PathBuf::from("/home/u/.local/share/applications"));
        assert_eq!(p.flatpak_user_dir, PathBuf::from("/home/u/.local/share/flatpak"));
        assert_eq!(p.flatpak_system_dirs, vec![PathBuf::from("/var/lib/flatpak")]);
        assert_eq!(
            p.default_instance_dir("com.slack.Slack", "work"),
            PathBuf::from("/home/u/.var/app/com.slack.Slack/instances/work")
        );
        assert_eq!(
            p.desktop_file("com.slack.Slack", "work"),
            PathBuf::from("/home/u/.local/share/applications/multipak-com.slack.Slack-work.desktop")
        );
    }

    #[test]
    fn relative_xdg_values_are_ignored() {
        let p = Paths::from_lookup(lookup(HashMap::from([
            ("HOME", "/home/u"),
            ("XDG_CONFIG_HOME", "relative/dir"),
            ("XDG_DATA_HOME", "/data"),
        ])))
        .unwrap();
        assert_eq!(p.xdg_config_home, PathBuf::from("/home/u/.config"));
        assert_eq!(p.xdg_data_home, PathBuf::from("/data"));
        assert_eq!(p.flatpak_user_dir, PathBuf::from("/data/flatpak"));
    }

    #[test]
    fn missing_home_is_an_error() {
        assert!(Paths::from_lookup(|_| None).is_err());
    }

    #[test]
    fn tilde_round_trip() {
        let p = Paths::for_home("/home/u");
        assert_eq!(p.tildify(Path::new("/home/u/Work/data")), "~/Work/data");
        assert_eq!(p.tildify(Path::new("/srv/data")), "/srv/data");
        assert_eq!(p.expand_tilde("~/Work/data"), PathBuf::from("/home/u/Work/data"));
        assert_eq!(p.expand_tilde("/srv/x"), PathBuf::from("/srv/x"));
    }
}
