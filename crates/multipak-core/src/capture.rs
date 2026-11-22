//! Hiding windows from screenshots, screen recording and screen sharing.
//!
//! Wayland has no standard protocol for this, so it depends on the compositor:
//!
//! - **KDE Plasma (KWin)** lets a window be excluded from capture (the "Exclude from screen
//!   capture" property).
//! - Other desktops (GNOME, wlroots-based compositors, …) offer no such setting; the option
//!   is saved but has no effect there.
//!
//! Instances are matched **per process**, not per window class, because many apps (most
//! GTK apps, for example) ignore any class Multipak passes and every copy of the app would
//! share one class. The pieces:
//!
//! 1. A KWin script (`~/.local/share/kwin/scripts/multipak-capture`) asks, for every window,
//!    whether its process belongs to a hidden instance, and sets `excludeFromCapture`.
//! 2. It asks over D-Bus. The answer comes from `multipak capture-service`, which D-Bus
//!    starts on demand through `~/.local/share/dbus-1/services/<name>.service` and which
//!    exits again when idle. It looks for `MULTIPAK_INSTANCE` in the environment of the
//!    window's process (or its parents) and checks that instance's setting.
//!
//! Both are installed only while at least one instance hides from capture. Multipak's own
//! window has a reliable class (`multipak`), so it uses a plain KWin window rule instead.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::{IoContext, Result};
use crate::exec::{CommandSpec, Host};
use crate::keyfile::KeyFile;
use crate::paths::Paths;

use crate::config::Config;
use crate::instance::{InstanceKey, MARKER_ENV};

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
            Self::KWin => "Supported on KDE Plasma (KWin).".into(),
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

/// Rule group name an instance used in older versions (class-based rules).
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

/// Leftover class-based rule groups for instances from older versions.
pub fn instance_groups(paths: &Paths) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(rules_file(paths)) else { return Vec::new() };
    KeyFile::parse(&text).groups().into_iter().filter(|g| g.starts_with(GROUP_PREFIX) && g != SELF_GROUP).collect()
}

/// Calls a session-bus method that takes only string arguments, using whichever D-Bus
/// command-line tool is installed (`gdbus`, `qdbus6`, `qdbus`, then `busctl`).
pub fn dbus_call(host: &dyn Host, dest: &str, path: &str, interface: &str, method: &str, args: &[&str]) -> Result<()> {
    let timeout = Duration::from_secs(10);
    let full = format!("{interface}.{method}");
    let mut attempts: Vec<(&str, Vec<String>)> = Vec::new();
    let mut gdbus: Vec<String> =
        ["call", "--session", "--dest", dest, "--object-path", path, "--method", &full].map(str::to_string).to_vec();
    // gdbus parses arguments as GVariant text, so strings are single-quoted.
    gdbus.extend(args.iter().map(|a| format!("'{}'", a.replace('\\', "\\\\").replace('\'', "\\'"))));
    attempts.push(("gdbus", gdbus));
    for tool in ["qdbus6", "qdbus"] {
        let mut v: Vec<String> = vec![dest.into(), path.into(), full.clone()];
        v.extend(args.iter().map(|a| a.to_string()));
        attempts.push((tool, v));
    }
    let mut busctl: Vec<String> =
        vec!["--user".into(), "call".into(), dest.into(), path.into(), interface.into(), method.into()];
    if !args.is_empty() {
        busctl.push("s".repeat(args.len()));
        busctl.extend(args.iter().map(|a| a.to_string()));
    }
    attempts.push(("busctl", busctl));
    let mut last_err = None;
    for (tool, argv) in attempts {
        let Some(bin) = host.which(tool) else { continue };
        let spec = CommandSpec::new(bin.display().to_string()).args(argv).timeout(timeout);
        match host.run(&spec).and_then(|o| o.check(&spec)) {
            Ok(_) => return Ok(()),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| {
        crate::Error::invalid("D-Bus", "no D-Bus command-line tool (gdbus, qdbus6, qdbus or busctl) was found")
    }))
}

/// Asks a running KWin to reload its configuration (window rules).
pub fn reconfigure_kwin(host: &dyn Host) -> Result<()> {
    dbus_call(host, "org.kde.KWin", "/KWin", "org.kde.KWin", "reconfigure", &[])
}

// ----- per-process helper ------------------------------------------------------------

/// D-Bus name, object path and interface of `multipak capture-service`.
pub const SERVICE_NAME: &str = "io.github.minasaad47.Multipak";
pub const SERVICE_PATH: &str = "/io/github/minasaad47/Multipak";
pub const SERVICE_INTERFACE: &str = "io.github.minasaad47.Multipak.Capture";
/// Plugin ID of the KWin script.
pub const SCRIPT_ID: &str = "multipak-capture";

/// Answers of [`decide`], as sent over D-Bus.
pub const DECISION_HIDE: i32 = 1;
pub const DECISION_SHOW: i32 = 0;
pub const DECISION_NOT_OURS: i32 = -1;

/// The instance a process belongs to, from `MULTIPAK_INSTANCE` in its environment or in
/// one of its parents (a window may be owned by a helper process of the app).
pub fn instance_for_pid(host: &dyn Host, pid: u32) -> Option<InstanceKey> {
    let prefix = format!("{MARKER_ENV}=");
    let mut current = pid;
    for _ in 0..8 {
        if current <= 1 {
            return None;
        }
        if let Some(env) = host.proc_environ(current)
            && let Some(key) =
                crate::exec::split_nul(&env).find_map(|v| v.strip_prefix(&prefix).and_then(InstanceKey::parse))
        {
            return Some(key);
        }
        current = host.proc_parent(current)?;
    }
    None
}

/// Whether the window of process `pid` should be hidden: [`DECISION_HIDE`],
/// [`DECISION_SHOW`] (a Multipak instance that is not hidden) or [`DECISION_NOT_OURS`].
pub fn decide(host: &dyn Host, config: &Config, pid: u32) -> i32 {
    match instance_for_pid(host, pid).and_then(|k| config.get(&k).map(|i| i.hide_from_capture)) {
        Some(true) => DECISION_HIDE,
        Some(false) => DECISION_SHOW,
        None => DECISION_NOT_OURS,
    }
}

/// `~/.local/share/kwin/scripts/multipak-capture`
pub fn script_dir(paths: &Paths) -> PathBuf {
    paths.xdg_data_home.join("kwin/scripts").join(SCRIPT_ID)
}

/// `~/.local/share/dbus-1/services/io.github.minasaad47.Multipak.service`
pub fn service_file(paths: &Paths) -> PathBuf {
    paths.xdg_data_home.join("dbus-1/services").join(format!("{SERVICE_NAME}.service"))
}

pub fn script_metadata() -> String {
    format!(
        r#"{{
    "KPlugin": {{
        "Authors": [{{ "Name": "Multipak" }}],
        "Description": "Hides windows of Multipak instances from screenshots, screen recordings and screen sharing",
        "EnabledByDefault": false,
        "Id": "{SCRIPT_ID}",
        "License": "MIT",
        "Name": "Multipak capture privacy",
        "Version": "1"
    }},
    "KPackageStructure": "KWin/Script",
    "X-Plasma-API": "javascript",
    "X-Plasma-MainScript": "code/main.js"
}}
"#
    )
}

pub fn script_main() -> String {
    format!(
        r#"// Installed and managed by Multipak. Hides windows of Multipak instances that have
// "Hide from screenshots, recordings and screen sharing" turned on.
// The answer comes from `multipak capture-service` over D-Bus (started on demand).
const SERVICE = "{SERVICE_NAME}";
const PATH = "{SERVICE_PATH}";
const IFACE = "{SERVICE_INTERFACE}";

function check(window) {{
    if (!window || !(window.pid > 0)) {{
        return;
    }}
    callDBus(SERVICE, PATH, IFACE, "ShouldHide", String(window.pid), function (decision) {{
        if (decision === {DECISION_HIDE}) {{
            window.excludeFromCapture = true;
        }} else if (decision === {DECISION_SHOW}) {{
            window.excludeFromCapture = false;
        }}
    }});
}}

workspace.windowAdded.connect(check);
workspace.windowList().forEach(check);
"#
    )
}

/// Contents of the D-Bus activation file for `helper` (the Multipak executable).
pub fn service_contents(helper: &Path) -> String {
    let quoted = helper.display().to_string().replace('\\', "\\\\").replace('"', "\\\"");
    format!("[D-BUS Service]\nName={SERVICE_NAME}\nExec=\"{quoted}\" capture-service\n")
}

fn write_if_changed(path: &Path, contents: &str, mode: u32) -> Result<bool> {
    if std::fs::read_to_string(path).is_ok_and(|c| c == contents) {
        return Ok(false);
    }
    crate::atomic::write_atomic(path, contents.as_bytes(), mode)?;
    Ok(true)
}

/// Whether the script, its D-Bus service file and the KWin plugin switch are all in place
/// and up to date for `helper`.
pub fn helper_installed(paths: &Paths, helper: &Path) -> bool {
    let dir = script_dir(paths);
    let same = |p: PathBuf, want: String| std::fs::read_to_string(p).is_ok_and(|c| c == want);
    same(dir.join("metadata.json"), script_metadata())
        && same(dir.join("contents/code/main.js"), script_main())
        && same(service_file(paths), service_contents(helper))
        && plugin_enabled(paths)
}

/// Whether any part of the helper is present.
pub fn helper_present(paths: &Paths) -> bool {
    script_dir(paths).exists() || service_file(paths).exists() || plugin_enabled(paths)
}

fn kwinrc(paths: &Paths) -> PathBuf {
    paths.xdg_config_home.join("kwinrc")
}

/// Whether the KWin script is switched on in `kwinrc`.
pub fn plugin_enabled(paths: &Paths) -> bool {
    std::fs::read_to_string(kwinrc(paths))
        .is_ok_and(|t| KeyFile::parse(&t).get_bool("Plugins", &format!("{SCRIPT_ID}Enabled")) == Some(true))
}

/// Switches the KWin script on or off in `kwinrc` (with `kwriteconfig6` when available,
/// so KConfig writes its own file).
fn set_plugin_enabled(host: &dyn Host, paths: &Paths, enabled: bool) -> Result<()> {
    let key = format!("{SCRIPT_ID}Enabled");
    let file = kwinrc(paths);
    if let Some(tool) = host.which("kwriteconfig6") {
        let mut spec = CommandSpec::new(tool.display().to_string()).args([
            "--file".to_string(),
            file.display().to_string(),
            "--group".into(),
            "Plugins".into(),
            "--key".into(),
            key,
        ]);
        spec = if enabled { spec.args(["--type", "bool", "true"]) } else { spec.args(["--delete"]) };
        let spec = spec.timeout(Duration::from_secs(10));
        host.run(&spec)?.check(&spec)?;
        return Ok(());
    }
    let text = std::fs::read_to_string(&file).unwrap_or_default();
    let mut kf = KeyFile::parse(&text);
    if enabled {
        kf.set("Plugins", &key, "true");
    } else if !kf.remove("Plugins", &key) {
        return Ok(());
    }
    crate::atomic::write_atomic(&file, kf.to_text().as_bytes(), 0o600)
}

/// (Re)loads the KWin script so every open window is checked again.
fn reload_script(host: &dyn Host) -> Result<()> {
    // Unloading fails harmlessly when the script is not loaded yet.
    let _ = dbus_call(host, "org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting", "unloadScript", &[SCRIPT_ID]);
    dbus_call(host, "org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting", "start", &[])
}

/// Installs (`wanted = true`) or removes the per-process helper, and reloads KWin so the
/// change applies to open windows immediately. `helper` is the Multipak executable.
pub fn sync_helper(host: &dyn Host, paths: &Paths, helper: &Path, wanted: bool) -> Result<()> {
    if wanted {
        let dir = script_dir(paths);
        write_if_changed(&dir.join("metadata.json"), &script_metadata(), 0o644)?;
        write_if_changed(&dir.join("contents/code/main.js"), &script_main(), 0o644)?;
        if write_if_changed(&service_file(paths), &service_contents(helper), 0o644)? {
            // dbus-broker only notices new activation files after a reload.
            let _ = dbus_call(
                host,
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "ReloadConfig",
                &[],
            );
        }
        if !plugin_enabled(paths) {
            set_plugin_enabled(host, paths, true)?;
        }
        reload_script(host)
    } else {
        if !helper_present(paths) {
            return Ok(());
        }
        let _ = dbus_call(host, "org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting", "unloadScript", &[SCRIPT_ID]);
        set_plugin_enabled(host, paths, false)?;
        let dir = script_dir(paths);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).ctx(|| format!("could not remove {}", dir.display()))?;
        }
        match std::fs::remove_file(service_file(paths)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).ctx(|| format!("could not remove {}", service_file(paths).display())),
        }
    }
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
    fn finds_instance_through_parent_processes() {
        use crate::fake::FakeProcess;
        let host = FakeHost::new();
        host.add_process(
            30,
            FakeProcess {
                environ: vec!["MULTIPAK_INSTANCE=com.x.Y/work".into()],
                parent: Some(20),
                ..Default::default()
            },
        );
        host.add_process(
            31,
            FakeProcess { environ: vec!["PATH=/app/bin".into()], parent: Some(30), ..Default::default() },
        );
        host.add_process(32, FakeProcess { parent: Some(31), ..Default::default() }); // environ unreadable
        host.add_process(
            40,
            FakeProcess { environ: vec!["FLATPAK_ID=com.x.Y".into()], parent: Some(1), ..Default::default() },
        );
        let key = InstanceKey::new("com.x.Y", "work");
        assert_eq!(instance_for_pid(&host, 32), Some(key.clone()));
        assert_eq!(instance_for_pid(&host, 40), None);
        assert_eq!(instance_for_pid(&host, 999), None);

        let paths = Paths::for_home("/home/u");
        let mut inst = crate::instance::InstanceDraft::new("com.x.Y", "Y", "work", crate::Strategy::Generic, &[])
            .validate(crate::instance::DraftContext { paths: &paths, existing: &[], editing: None })
            .unwrap()
            .instance;
        let mut config = Config { instances: vec![inst.clone()], ..Default::default() };
        assert_eq!(decide(&host, &config, 32), DECISION_SHOW);
        inst.hide_from_capture = true;
        config.instances = vec![inst];
        assert_eq!(decide(&host, &config, 32), DECISION_HIDE);
        assert_eq!(decide(&host, &config, 40), DECISION_NOT_OURS);
    }

    #[test]
    fn installs_and_removes_helper() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::for_home(dir.path());
        let host = FakeHost::new();
        host.add_executable("gdbus", "/usr/bin/gdbus");
        let exe = Path::new("/opt/My Apps/multipak");
        assert!(!helper_present(&paths));
        sync_helper(&host, &paths, exe, true).unwrap();
        assert!(helper_installed(&paths, exe));
        let service = std::fs::read_to_string(service_file(&paths)).unwrap();
        assert!(service.contains(r#"Exec="/opt/My Apps/multipak" capture-service"#));
        assert!(
            std::fs::read_to_string(script_dir(&paths).join("contents/code/main.js"))
                .unwrap()
                .contains("excludeFromCapture")
        );
        let kwinrc = std::fs::read_to_string(paths.xdg_config_home.join("kwinrc")).unwrap();
        assert!(kwinrc.contains("[Plugins]\nmultipak-captureEnabled=true"));
        let methods: Vec<String> = host.calls().iter().filter_map(|c| c.args.get(7).cloned()).collect();
        assert!(methods.contains(&"org.freedesktop.DBus.ReloadConfig".to_string()));
        assert!(methods.contains(&"org.kde.kwin.Scripting.start".to_string()));
        assert!(!helper_installed(&paths, Path::new("/elsewhere/multipak")), "a moved executable is detected");

        sync_helper(&host, &paths, exe, false).unwrap();
        assert!(!helper_present(&paths));
        assert!(!std::fs::read_to_string(paths.xdg_config_home.join("kwinrc")).unwrap().contains("multipak-capture"));
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
