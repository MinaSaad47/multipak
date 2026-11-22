//! Queries and actions on the host's Flatpak installation, all through [`Host`].

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::{Error, IoContext, Result};
use crate::exec::{CommandSpec, Host};
use crate::keyfile::KeyFile;
use crate::paths::Paths;

/// The program name used for every Flatpak call.
pub const FLATPAK: &str = "flatpak";

const QUERY_TIMEOUT: Duration = Duration::from_secs(30);

/// An installed Flatpak application.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FlatpakApp {
    pub app_id: String,
    pub name: String,
    pub origin: String,
    /// `user`, `system` or the name of a custom installation.
    pub installation: String,
}

/// One running sandbox from `flatpak ps`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PsEntry {
    pub instance: String,
    pub pid: u32,
    pub child_pid: u32,
    pub app_id: String,
}

/// The parts of an app's metadata Multipak uses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppMetadata {
    pub runtime: Option<String>,
    pub base: Option<String>,
    pub command: Option<String>,
    pub filesystems: Vec<String>,
}

/// Parses `flatpak list --app --columns=application,name,origin,installation`.
/// The same app may be installed in several installations; each appears once per installation.
pub fn parse_list(stdout: &str) -> Vec<FlatpakApp> {
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| {
            let mut cols = line.split('\t');
            let app_id = cols.next()?.trim().to_string();
            if app_id.is_empty() || app_id == "Application ID" {
                return None;
            }
            let name = cols.next().unwrap_or("").trim().to_string();
            let origin = cols.next().unwrap_or("").trim().to_string();
            let installation = cols.next().unwrap_or("").trim().to_string();
            Some(FlatpakApp { name: if name.is_empty() { app_id.clone() } else { name }, app_id, origin, installation })
        })
        .collect()
}

/// Parses `flatpak ps --columns=instance,pid,child-pid,application`.
pub fn parse_ps(stdout: &str) -> Vec<PsEntry> {
    stdout
        .lines()
        .filter_map(|line| {
            let cols: Vec<&str> = line.split('\t').map(str::trim).collect();
            if cols.len() < 4 {
                return None;
            }
            Some(PsEntry {
                instance: cols[0].to_string(),
                pid: cols[1].parse().ok()?,
                child_pid: cols[2].parse().ok()?,
                app_id: cols[3].to_string(),
            })
        })
        .collect()
}

/// Parses the output of `flatpak info --show-metadata`.
pub fn parse_metadata(text: &str) -> AppMetadata {
    let kf = KeyFile::parse(text);
    AppMetadata {
        runtime: kf.get("Application", "runtime").map(str::to_string),
        base: kf.get("Application", "base").map(str::to_string),
        command: kf.get("Application", "command").map(str::to_string),
        filesystems: kf.get_list("Context", "filesystems"),
    }
}

/// Thin wrapper around the `flatpak` command.
pub struct Flatpak<'a> {
    host: &'a dyn Host,
}

impl<'a> Flatpak<'a> {
    pub fn new(host: &'a dyn Host) -> Self {
        Self { host }
    }

    fn spec(args: &[&str]) -> CommandSpec {
        CommandSpec::new(FLATPAK).args(args.iter().copied()).timeout(QUERY_TIMEOUT)
    }

    fn query(&self, args: &[&str]) -> Result<String> {
        if self.host.which(FLATPAK).is_none() {
            return Err(Error::FlatpakMissing);
        }
        let spec = Self::spec(args);
        Ok(self.host.run(&spec)?.check(&spec)?.stdout)
    }

    /// Absolute path of the `flatpak` binary, used in launchers.
    pub fn binary(&self) -> Result<PathBuf> {
        self.host.which(FLATPAK).ok_or(Error::FlatpakMissing)
    }

    /// Installed apps from all installations, sorted by name.
    pub fn list_apps(&self) -> Result<Vec<FlatpakApp>> {
        let out = self.query(&["list", "--app", "--columns=application,name,origin,installation"])?;
        let mut apps = parse_list(&out);
        apps.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.app_id.cmp(&b.app_id))
                .then_with(|| a.installation.cmp(&b.installation))
        });
        Ok(apps)
    }

    /// Running sandboxes.
    pub fn ps(&self) -> Result<Vec<PsEntry>> {
        let out = self.query(&["ps", "--columns=instance,pid,child-pid,application"])?;
        Ok(parse_ps(&out))
    }

    /// Stops one running sandbox.
    pub fn kill(&self, instance: &str) -> Result<()> {
        self.query(&["kill", instance]).map(|_| ())
    }

    /// Metadata of an installed app.
    pub fn metadata(&self, app_id: &str) -> Result<AppMetadata> {
        Ok(parse_metadata(&self.query(&["info", "--show-metadata", app_id])?))
    }

    /// Deployment folder of an installed app (its `files/` folder holds the app).
    pub fn location(&self, app_id: &str) -> Result<PathBuf> {
        Ok(PathBuf::from(self.query(&["info", "--show-location", app_id])?.trim()))
    }

    /// Whether the app is installed in any installation.
    pub fn is_installed(&self, app_id: &str) -> Result<bool> {
        if self.host.which(FLATPAK).is_none() {
            return Err(Error::FlatpakMissing);
        }
        let spec = Self::spec(&["info", "--show-ref", app_id]);
        Ok(self.host.run(&spec)?.success())
    }

    /// Adds `--filesystem=<dir>` to the app's per-user overrides.
    pub fn add_filesystem_override(&self, app_id: &str, dir: &str) -> Result<()> {
        self.query(&["override", "--user", &format!("--filesystem={dir}"), app_id]).map(|_| ())
    }
}

fn filesystem_entry_matches(entry: &str, dir: &str) -> bool {
    let entry = entry.trim_start_matches('!');
    let path = match entry.rsplit_once(':') {
        Some((p, "ro" | "rw" | "create")) => p,
        _ => entry,
    };
    path.trim_end_matches('/') == dir.trim_end_matches('/')
}

/// Whether the per-user override file of `app_id` already grants `dir`.
pub fn override_has_filesystem(paths: &Paths, app_id: &str, dir: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(paths.overrides_file(app_id)) else {
        return false;
    };
    KeyFile::parse(&text)
        .get_list("Context", "filesystems")
        .iter()
        .any(|e| !e.starts_with('!') && filesystem_entry_matches(e, dir))
}

/// Removes exactly one `filesystems` entry that Multipak added, leaving every other
/// override intact. `flatpak override --nofilesystem` would instead add a *negative*
/// entry that blocks access the app's manifest grants, so the keyfile is edited directly.
pub fn remove_filesystem_override(paths: &Paths, app_id: &str, dir: &str) -> Result<bool> {
    let file = paths.overrides_file(app_id);
    let text = match std::fs::read_to_string(&file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).ctx(|| format!("could not read {}", file.display())),
    };
    let mut kf = KeyFile::parse(&text);
    let entries = kf.get_list("Context", "filesystems");
    let kept: Vec<&String> =
        entries.iter().filter(|e| e.starts_with('!') || !filesystem_entry_matches(e, dir)).collect();
    if kept.len() == entries.len() {
        return Ok(false);
    }
    if kept.is_empty() {
        kf.remove("Context", "filesystems");
    } else {
        let joined: String = kept.iter().map(|e| format!("{e};")).collect();
        kf.set("Context", "filesystems", &joined);
    }
    crate::atomic::write_atomic(&file, kf.to_text().as_bytes(), 0o644)?;
    Ok(true)
}

/// Whether a path is inside the folder the sandbox of `app_id` can always reach.
pub fn is_inside_app_dir(paths: &Paths, app_id: &str, dir: &Path) -> bool {
    dir.starts_with(paths.app_var_dir(app_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_list_output() {
        let out = "com.slack.Slack\tSlack\tflathub\tuser\norg.mozilla.firefox\tFirefox\tflathub\tsystem\ncom.example.NoName\t\tlocal\tcustom-inst\n\n";
        let apps = parse_list(out);
        assert_eq!(apps.len(), 3);
        assert_eq!(apps[0].app_id, "com.slack.Slack");
        assert_eq!(apps[1].installation, "system");
        assert_eq!(apps[2].name, "com.example.NoName");
    }

    #[test]
    fn parses_ps_output() {
        let out = "1410889120\t10414\t10424\tapp.zen_browser.zen\nbad line\n";
        assert_eq!(
            parse_ps(out),
            vec![PsEntry {
                instance: "1410889120".into(),
                pid: 10414,
                child_pid: 10424,
                app_id: "app.zen_browser.zen".into()
            }]
        );
    }

    #[test]
    fn parses_metadata() {
        let md = parse_metadata(
            "[Application]\nname=com.slack.Slack\nruntime=org.freedesktop.Platform/x86_64/25.08\nbase=app/org.electronjs.Electron2.BaseApp/x86_64/25.08\ncommand=com.slack.Slack\n\n[Context]\nfilesystems=xdg-download;\n",
        );
        assert_eq!(md.base.as_deref(), Some("app/org.electronjs.Electron2.BaseApp/x86_64/25.08"));
        assert_eq!(md.filesystems, vec!["xdg-download"]);
    }

    #[test]
    fn override_removal_keeps_other_entries() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::for_home(dir.path());
        let file = paths.overrides_file("com.example.App");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "[Context]\nfilesystems=xdg-music;/srv/work:rw;!/srv/blocked;\nsockets=x11;\n").unwrap();
        assert!(override_has_filesystem(&paths, "com.example.App", "/srv/work"));
        assert!(!override_has_filesystem(&paths, "com.example.App", "/srv/blocked"));
        assert!(remove_filesystem_override(&paths, "com.example.App", "/srv/work/").unwrap());
        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(text, "[Context]\nfilesystems=xdg-music;!/srv/blocked;\nsockets=x11;\n");
        assert!(!remove_filesystem_override(&paths, "com.example.App", "/srv/work").unwrap());
        assert!(!remove_filesystem_override(&paths, "com.other.App", "/srv/work").unwrap());
    }
}
