//! Hiding windows from screenshots, screen recording and screen sharing.
//!
//! Wayland has no standard protocol for this, so it depends on the compositor:
//!
//! - **KDE Plasma (KWin)** supports it through window rules: a rule with
//!   `excludefromcapture=true` (the "Exclude from screen capture" property in System
//!   Settings → Window Management → Window Rules). Multipak writes one rule group per
//!   window class into `~/.config/kwinrulesrc`, named `multipak-…`, and asks KWin to reload
//!   its configuration. Rules the user created are never touched.
//! - Other desktops (GNOME, wlroots-based compositors, …) offer no such setting; the option
//!   is saved but has no effect there.
//!
//! Matching is by window class (the Wayland app ID or X11 `WM_CLASS`), which is why each
//! instance's window class matters.

use std::ffi::OsString;
use std::time::Duration;

use crate::error::{IoContext, Result};
use crate::exec::{CommandSpec, Host};
use crate::keyfile::KeyFile;
use crate::paths::Paths;

/// Rule group used for Multipak's own window.
pub const SELF_GROUP: &str = "multipak-self";
/// Window class of Multipak's own window.
pub const SELF_WM_CLASS: &str = "multipak";
const GROUP_PREFIX: &str = "multipak-";
/// KWin rule policy "Force": always applies and cannot be changed from the window menu.
const POLICY_FORCE: &str = "2";

/// Whether the current desktop can hide windows from capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Support {
    /// KDE Plasma: handled with KWin window rules.
    KWin,
    /// The desktop has no known way to do it (the name is for messages).
    Unsupported(String),
}

impl Support {
    pub fn is_supported(&self) -> bool {
        matches!(self, Self::KWin)
    }

    /// A one-line explanation for the UI.
    pub fn description(&self) -> String {
        match self {
            Self::KWin => "Supported on KDE Plasma through a KWin window rule.".into(),
            Self::Unsupported(desktop) => format!(
                "Your desktop ({desktop}) cannot hide windows from capture; only KDE Plasma supports this. The setting is saved and works when you use Plasma."
            ),
        }
    }
}

/// Detects support from the session environment (`XDG_CURRENT_DESKTOP`).
pub fn detect(lookup: impl Fn(&str) -> Option<OsString>) -> Support {
    let desktop = lookup("XDG_CURRENT_DESKTOP").map(|v| v.to_string_lossy().into_owned()).unwrap_or_default();
    if desktop.split(':').any(|d| d.eq_ignore_ascii_case("KDE")) {
        Support::KWin
    } else if desktop.is_empty() {
        Support::Unsupported("unknown desktop".into())
    } else {
        Support::Unsupported(desktop)
    }
}

/// `~/.config/kwinrulesrc`
pub fn rules_file(paths: &Paths) -> std::path::PathBuf {
    paths.xdg_config_home.join("kwinrulesrc")
}

/// Rule group name for one instance.
pub fn instance_group(app_id: &str, slug: &str) -> String {
    format!("{GROUP_PREFIX}{app_id}-{slug}")
}

fn order(kf: &KeyFile) -> Vec<String> {
    kf.get("General", "Order")
        .map(|v| v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect())
        .unwrap_or_default()
}

fn set_order(kf: &mut KeyFile, ids: &[String]) {
    kf.set("General", "Order", &ids.join(","));
    // Older KWin versions also read a rule count.
    if kf.get("General", "count").is_some() {
        kf.set("General", "count", &ids.len().to_string());
    }
}

/// The rule Multipak expects for a window class.
fn expected_rule(wm_class: &str, description: &str) -> [(&'static str, String); 5] {
    [
        ("Description", description.replace(['\n', '\r'], " ")),
        ("excludefromcapture", "true".into()),
        ("excludefromcapturerule", POLICY_FORCE.into()),
        ("wmclass", wm_class.into()),
        ("wmclassmatch", "1".into()),
    ]
}

/// Whether `group` exists with exactly the expected rule.
pub fn has_rule(paths: &Paths, group: &str, wm_class: &str, description: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(rules_file(paths)) else { return false };
    let kf = KeyFile::parse(&text);
    order(&kf).iter().any(|g| g == group)
        && expected_rule(wm_class, description).iter().all(|(k, v)| kf.get(group, k) == Some(v.as_str()))
}

/// Adds, updates (`enabled = true`) or removes (`enabled = false`) Multipak's rule group.
/// Returns whether the file changed.
pub fn set_rule(paths: &Paths, group: &str, wm_class: &str, description: &str, enabled: bool) -> Result<bool> {
    assert!(group.starts_with(GROUP_PREFIX), "Multipak only edits its own rule groups");
    let file = rules_file(paths);
    let text = match std::fs::read_to_string(&file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if !enabled {
                return Ok(false);
            }
            String::new()
        }
        Err(e) => return Err(e).ctx(|| format!("could not read {}", file.display())),
    };
    let mut kf = KeyFile::parse(&text);
    let mut ids = order(&kf);
    if enabled {
        // Replace the group so stale keys from an older version never linger.
        kf.remove_group(group);
        for (k, v) in expected_rule(wm_class, description) {
            kf.set(group, k, &v);
        }
        if !ids.iter().any(|g| g == group) {
            ids.push(group.to_string());
        }
    } else {
        kf.remove_group(group);
        ids.retain(|g| g != group);
    }
    set_order(&mut kf, &ids);
    let new_text = kf.to_text();
    if new_text == text {
        return Ok(false);
    }
    crate::atomic::write_atomic(&file, new_text.as_bytes(), 0o600)?;
    Ok(true)
}

/// Multipak rule groups for instances (excluding Multipak's own window).
pub fn instance_groups(paths: &Paths) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(rules_file(paths)) else { return Vec::new() };
    KeyFile::parse(&text).groups().into_iter().filter(|g| g.starts_with(GROUP_PREFIX) && g != SELF_GROUP).collect()
}

/// Asks a running KWin to reload its rules. Tries `gdbus`, `qdbus6`, `qdbus`, then `busctl`.
pub fn reconfigure_kwin(host: &dyn Host) -> Result<()> {
    let timeout = Duration::from_secs(10);
    let attempts: [(&str, &[&str]); 4] = [
        (
            "gdbus",
            &[
                "call",
                "--session",
                "--dest",
                "org.kde.KWin",
                "--object-path",
                "/KWin",
                "--method",
                "org.kde.KWin.reconfigure",
            ],
        ),
        ("qdbus6", &["org.kde.KWin", "/KWin", "reconfigure"]),
        ("qdbus", &["org.kde.KWin", "/KWin", "reconfigure"]),
        ("busctl", &["--user", "call", "org.kde.KWin", "/KWin", "org.kde.KWin", "reconfigure"]),
    ];
    let mut last_err = None;
    for (tool, args) in attempts {
        let Some(bin) = host.which(tool) else { continue };
        let spec = CommandSpec::new(bin.display().to_string()).args(args.iter().copied()).timeout(timeout);
        match host.run(&spec).and_then(|o| o.check(&spec)) {
            Ok(_) => return Ok(()),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| {
        crate::Error::invalid("D-Bus", "no D-Bus command-line tool (gdbus, qdbus6, qdbus or busctl) was found")
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::CmdOutput;
    use crate::fake::FakeHost;

    const USER_RULES: &str = "[$Version]\nupdate_info=kwinrules.upd:use-virtual-desktop-ids\n\n[1]\nDescription=Application settings for Alacritty\nexcludefromcapture=true\nexcludefromcapturerule=3\nwmclass=alacritty Alacritty\nwmclassmatch=1\n\n[General]\nOrder=1\n";

    #[test]
    fn detects_plasma() {
        let env = |v: &'static str| move |k: &str| (k == "XDG_CURRENT_DESKTOP").then(|| OsString::from(v));
        assert_eq!(detect(env("KDE")), Support::KWin);
        assert_eq!(detect(env("ubuntu:KDE")), Support::KWin);
        assert_eq!(detect(env("GNOME")), Support::Unsupported("GNOME".into()));
        assert!(!detect(|_| None).is_supported());
    }

    #[test]
    fn adds_updates_and_removes_only_its_rule() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::for_home(dir.path());
        let file = rules_file(&paths);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, USER_RULES).unwrap();
        let g = instance_group("com.slack.Slack", "work");

        assert!(set_rule(&paths, &g, "multipak-slack-work", "Multipak: Slack (Work)", true).unwrap());
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("[multipak-com.slack.Slack-work]\nDescription=Multipak: Slack (Work)\nexcludefromcapture=true\nexcludefromcapturerule=2\nwmclass=multipak-slack-work\nwmclassmatch=1\n"));
        assert!(text.contains("Order=1,multipak-com.slack.Slack-work"));
        assert!(text.contains("[1]\nDescription=Application settings for Alacritty"));
        assert!(has_rule(&paths, &g, "multipak-slack-work", "Multipak: Slack (Work)"));
        assert!(!set_rule(&paths, &g, "multipak-slack-work", "Multipak: Slack (Work)", true).unwrap(), "idempotent");

        assert!(set_rule(&paths, &g, "multipak-slack-other", "Multipak: Slack (Work)", true).unwrap());
        assert!(!has_rule(&paths, &g, "multipak-slack-work", "Multipak: Slack (Work)"));
        assert_eq!(std::fs::read_to_string(&file).unwrap().matches("[multipak-").count(), 1);

        assert!(set_rule(&paths, &g, "", "", false).unwrap());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), USER_RULES);
        assert!(!set_rule(&paths, &g, "", "", false).unwrap());
    }

    #[test]
    fn creates_the_file_when_needed() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::for_home(dir.path());
        assert!(!set_rule(&paths, SELF_GROUP, SELF_WM_CLASS, "Multipak", false).unwrap());
        assert!(!rules_file(&paths).exists());
        assert!(set_rule(&paths, SELF_GROUP, SELF_WM_CLASS, "Multipak", true).unwrap());
        assert!(has_rule(&paths, SELF_GROUP, SELF_WM_CLASS, "Multipak"));
        assert!(instance_groups(&paths).is_empty());
    }

    #[test]
    fn reconfigure_uses_available_tool() {
        let host = FakeHost::new();
        assert!(reconfigure_kwin(&host).is_err());
        host.add_executable("busctl", "/usr/bin/busctl");
        reconfigure_kwin(&host).unwrap();
        assert_eq!(host.calls()[0].args[0], "--user");
        host.add_executable("gdbus", "/usr/bin/gdbus");
        host.on("gdbus", &["call"], CmdOutput::failed(1, "no KWin"));
        reconfigure_kwin(&host).unwrap();
        let calls = host.calls();
        assert!(calls[calls.len() - 2].program.ends_with("gdbus"));
        assert!(calls[calls.len() - 1].program.ends_with("busctl"), "falls back after a failure");
    }
}
