//! End-to-end tests of `Manager` with a temporary HOME and a fake `flatpak`.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use multipak_core::backup::CancelToken;
use multipak_core::exec::{CmdOutput, LaunchOutcome};
use multipak_core::fake::{FakeHost, FakeProcess};
use multipak_core::keyfile::KeyFile;
use multipak_core::manager::RestoreTarget;
use multipak_core::transfer::{ImportOptions, OnConflict};
use multipak_core::{Badge, Error, Fix, InstanceDraft, InstanceKey, IssueKind, Manager, Paths, Strategy};

struct Env {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    host: FakeHost,
    mgr: Manager,
}

const LIST: &str = "com.slack.Slack\tSlack\tflathub\tuser\norg.mozilla.firefox\tFirefox\tflathub\tsystem\n";

fn setup() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    env_with(tmp, home)
}

fn env_with(tmp: tempfile::TempDir, home: PathBuf) -> Env {
    let paths = Paths::for_home(&home);
    let host = FakeHost::new();
    host.on("flatpak", &["list"], CmdOutput::ok(LIST));
    host.on("flatpak", &["info", "--show-ref"], CmdOutput::ok("app/x/x86_64/stable\n"));
    host.on("flatpak", &["info", "--show-location"], CmdOutput::failed(1, "not found"));
    host.on(
        "flatpak",
        &["info", "--show-metadata"],
        CmdOutput::ok("[Application]\nruntime=org.freedesktop.Platform/x86_64/24.08\n"),
    );
    host.on("flatpak", &["ps"], CmdOutput::ok(""));
    // Imitate `flatpak override --user --filesystem=DIR APP` writing its keyfile.
    let overrides = paths.flatpak_user_dir.join("overrides");
    host.on_with("flatpak", &["override", "--user"], move |spec| {
        let dir = spec.args[2].strip_prefix("--filesystem=").unwrap().to_string();
        let file = overrides.join(&spec.args[3]);
        fs::create_dir_all(&overrides).unwrap();
        let mut kf = KeyFile::parse(&fs::read_to_string(&file).unwrap_or_default());
        let mut list = kf.get("Context", "filesystems").unwrap_or("").to_string();
        list.push_str(&format!("{dir};"));
        kf.set("Context", "filesystems", &list);
        fs::write(&file, kf.to_text()).unwrap();
        CmdOutput::ok("")
    });
    // Export the app's desktop file and icon like Flatpak does.
    let share = paths.flatpak_user_dir.join("exports/share");
    fs::create_dir_all(share.join("applications")).unwrap();
    fs::write(
        share.join("applications/com.slack.Slack.desktop"),
        "[Desktop Entry]\nName=Slack\nCategories=Network;InstantMessaging;\n",
    )
    .unwrap();
    let icon = share.join("icons/hicolor/scalable/apps/com.slack.Slack.svg");
    fs::create_dir_all(icon.parent().unwrap()).unwrap();
    fs::write(&icon, r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><rect width="16" height="16" fill="#4a154b"/></svg>"##).unwrap();
    let mut mgr = Manager::new(Arc::new(host.clone()), paths).unwrap();
    mgr.set_capture_support(multipak_core::capture::Support::KWin);
    mgr.set_capture_helper(PathBuf::from("/usr/bin/multipak"));
    Env { _tmp: tmp, home, host, mgr }
}

fn slack(name: &str, env: &Env) -> InstanceDraft {
    let config = env.mgr.load().unwrap();
    InstanceDraft::new("com.slack.Slack", "Slack", name, Strategy::Electron, &config.instances)
}

fn key(slug: &str) -> InstanceKey {
    InstanceKey::new("com.slack.Slack", slug)
}

#[test]
fn create_writes_launcher_and_data_dir() {
    let env = setup();
    let out = env.mgr.create(&slack("Work", &env)).unwrap();
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    let paths = env.mgr.paths();
    let data = env.home.join(".var/app/com.slack.Slack/instances/work");
    assert!(data.is_dir());
    let desktop = paths.desktop_file("com.slack.Slack", "work");
    let kf = KeyFile::parse(&fs::read_to_string(&desktop).unwrap());
    let g = "Desktop Entry";
    assert_eq!(kf.get(g, "Name"), Some("Slack (Work)"));
    assert_eq!(kf.get(g, "StartupWMClass"), Some("multipak-slack-work"));
    assert_eq!(kf.get(g, "Icon"), Some("com.slack.Slack"));
    assert_eq!(kf.get(g, "Categories"), Some("Network;InstantMessaging;"));
    let exec = multipak_core::desktop::parse_exec_value(kf.get(g, "Exec").unwrap());
    let app_dir = env.home.join(".var/app/com.slack.Slack");
    assert_eq!(
        exec[..8],
        [
            "/usr/bin/bwrap",
            "--dev-bind",
            "/",
            "/",
            "--bind",
            &data.display().to_string(),
            &app_dir.display().to_string(),
            "--"
        ]
    );
    assert_eq!(exec[8], "/usr/bin/flatpak");
    assert!(!exec.iter().any(|a| a.contains("XDG_CONFIG_HOME")));
    assert!(multipak_core::layout::is_current(&data));
    assert!(!kf.get(g, "Exec").unwrap().contains("$HOME"));
    assert_eq!(
        env.host.calls_matching(&[&paths.applications_dir().display().to_string()]).len(),
        1,
        "update-desktop-database runs"
    );
    let cfg = env.mgr.load().unwrap();
    assert_eq!(cfg.instances.len(), 1);
    assert!(matches!(env.mgr.create(&slack("x", &env).clone_with_slug("work")), Err(Error::Invalid { .. })));
}

trait WithSlug {
    fn clone_with_slug(&self, slug: &str) -> Self;
}
impl WithSlug for InstanceDraft {
    fn clone_with_slug(&self, slug: &str) -> Self {
        let mut d = self.clone();
        d.slug = slug.into();
        d
    }
}

#[test]
fn update_database_is_optional() {
    let env = setup();
    env.host.remove_executable("update-desktop-database");
    env.mgr.create(&slack("Work", &env)).unwrap();
    assert!(env.host.calls().iter().all(|c| !c.program.contains("update-desktop-database")));
}

#[test]
fn custom_dir_is_mounted_without_extra_permissions() {
    let env = setup();
    let custom = env.home.join("Clients/acme");
    let mut d = slack("Acme", &env);
    d.data_dir = Some(custom.clone());
    let inst = env.mgr.create(&d).unwrap().instance;
    assert_eq!(inst.managed_override, None);
    assert!(custom.is_dir());
    assert!(!env.mgr.paths().overrides_file("com.slack.Slack").exists());
    assert!(env.host.calls_matching(&["override"]).is_empty());
    env.mgr.launch(&inst.key()).unwrap();
    let spec = env.host.launches().pop().unwrap();
    let app_dir = env.mgr.paths().app_var_dir("com.slack.Slack");
    assert_eq!(spec.args[3..6], ["--bind".to_string(), custom.display().to_string(), app_dir.display().to_string()]);
}

#[test]
fn old_folder_access_is_removed() {
    let env = setup();
    let custom = env.home.join("Shared/slack");
    let mut d = slack("Shared", &env);
    d.data_dir = Some(custom.clone());
    let inst = env.mgr.create(&d).unwrap().instance;
    // What Multipak 0.1 left behind: a granted override, recorded on the instance, next to
    // one the user added themselves.
    let overrides = env.mgr.paths().overrides_file("com.slack.Slack");
    fs::create_dir_all(overrides.parent().unwrap()).unwrap();
    fs::write(&overrides, format!("[Context]\nfilesystems={};/srv/mine;\n", custom.display())).unwrap();
    let store = multipak_core::ConfigStore::new(env.mgr.paths().config_file());
    store
        .update(|c| {
            c.get_mut(&inst.key()).unwrap().managed_override = Some(custom.display().to_string());
            Ok(())
        })
        .unwrap();
    let issues = env.mgr.doctor().unwrap();
    assert!(
        issues.iter().any(|i| i.kind == IssueKind::OldOverride && i.fix == Some(Fix::RemoveOldOverride(inst.key())))
    );

    env.mgr.upgrade().unwrap();
    let text = fs::read_to_string(&overrides).unwrap();
    assert!(!text.contains(custom.to_str().unwrap()) && text.contains("/srv/mine"), "{text}");
    assert_eq!(env.mgr.load().unwrap().get(&inst.key()).unwrap().managed_override, None);
    assert!(!env.mgr.doctor().unwrap().iter().any(|i| i.kind == IssueKind::OldOverride));
}

#[test]
fn delete_data_is_guarded() {
    let env = setup();
    let inst = env.mgr.create(&slack("Work", &env)).unwrap().instance;
    let data = inst.resolved_data_dir(env.mgr.paths());
    fs::write(data.join("file"), "x").unwrap();
    let report = env.mgr.delete(&inst.key(), true).unwrap();
    assert!(report.data_deleted);
    assert!(!data.exists());
    assert!(env.mgr.paths().app_var_dir("com.slack.Slack").exists(), "only the instance folder is removed");
    assert!(!env.mgr.paths().desktop_file("com.slack.Slack", "work").exists());
}

#[test]
fn launch_builds_command_and_reports_failures() {
    let env = setup();
    let inst = env.mgr.create(&slack("Work", &env)).unwrap().instance;
    assert!(matches!(env.mgr.launch(&inst.key()).unwrap(), LaunchOutcome::Running { .. }));
    let spec = env.host.launches().pop().unwrap();
    assert_eq!(spec.program, "/usr/bin/bwrap");
    assert_eq!(spec.args[7], "/usr/bin/flatpak");
    assert_eq!(spec.args[9], "--env=MULTIPAK_INSTANCE=com.slack.Slack/work");
    assert!(spec.args.contains(&"--class=multipak-slack-work".to_string()));

    env.host.set_launch_outcome(LaunchOutcome::Failed { code: Some(1), log_tail: "error: app not installed".into() });
    let err = env.mgr.launch(&inst.key()).unwrap_err();
    assert!(err.to_string().contains("app not installed"), "{err}");
    assert!(matches!(env.mgr.launch(&key("nope")), Err(Error::InstanceNotFound(_))));
}

#[test]
fn firefox_uses_its_default_profile_inside_the_instance() {
    let env = setup();
    let d = InstanceDraft::new("org.mozilla.firefox", "Firefox", "Client A", Strategy::Firefox, &[]);
    let inst = env.mgr.create(&d).unwrap().instance;
    env.mgr.launch(&inst.key()).unwrap();
    let spec = env.host.launches().pop().unwrap();
    assert!(!spec.args.iter().any(|a| a.contains("profile")), "{:?}", spec.args);
    assert!(spec.args.contains(&"--new-instance".to_string()));
}

#[test]
fn launch_moves_old_data_into_place_once() {
    let env = setup();
    let inst = env.mgr.create(&slack("Work", &env)).unwrap().instance;
    let dir = inst.resolved_data_dir(env.mgr.paths());
    // A data folder written by Multipak 0.1, where it was Slack's XDG_CONFIG_HOME.
    fs::remove_file(dir.join(multipak_core::layout::MARKER)).unwrap();
    fs::create_dir_all(dir.join("Slack")).unwrap();
    fs::write(dir.join("Slack/Cookies"), "x").unwrap();
    assert!(env.mgr.upgrade().unwrap().is_empty());
    assert!(dir.join("config/Slack/Cookies").is_file());
    env.mgr.launch(&inst.key()).unwrap();
    assert!(dir.join("config/Slack/Cookies").is_file());
}

#[test]
fn running_detection_and_stop() {
    let env = setup();
    let work = env.mgr.create(&slack("Work", &env)).unwrap().instance;
    let home = env.mgr.create(&slack("Home", &env)).unwrap().instance;
    env.host.on(
        "flatpak",
        &["ps"],
        CmdOutput::ok("111\t10\t11\tcom.slack.Slack\n222\t20\t21\tcom.slack.Slack\n333\t30\t31\tcom.slack.Slack\n"),
    );
    // Sandbox 111: marker in the environment of a grandchild (bwrap hides its own env).
    env.host.add_process(11, FakeProcess { children: vec![12], ..Default::default() });
    env.host.add_process(
        12,
        FakeProcess {
            environ: vec!["FLATPAK_ID=com.slack.Slack".into(), "MULTIPAK_INSTANCE=com.slack.Slack/work".into()],
            ..Default::default()
        },
    );
    // Sandbox 222: marker directly on the app process.
    env.host.add_process(
        21,
        FakeProcess { environ: vec![format!("MULTIPAK_INSTANCE={}", home.key())], ..Default::default() },
    );
    // Sandbox 333: the app's normal launcher. Instance arguments alone do not count.
    env.host.add_process(
        31,
        FakeProcess { cmdline: vec!["/app/slack".into(), format!("--class={}", home.wm_class)], ..Default::default() },
    );

    let running = env.mgr.running().unwrap();
    assert_eq!(running[0].key.as_ref(), Some(&work.key()));
    assert_eq!(running[1].key.as_ref(), Some(&home.key()));
    assert_eq!(running[2].key, None);
    assert!(env.mgr.is_running(&work.key()).unwrap());

    assert_eq!(env.mgr.stop(&work.key()).unwrap(), 1);
    assert_eq!(env.host.calls_matching(&["kill"]).len(), 1);
    assert_eq!(env.host.calls_matching(&["kill"])[0].args[1], "111");

    assert!(matches!(env.mgr.delete(&work.key(), true), Err(Error::InstanceRunning(_))));
    assert!(matches!(env.mgr.clear_cache(&work.key(), &CancelToken::new()), Err(Error::InstanceRunning(_))));
}

#[test]
fn update_and_duplicate() {
    let env = setup();
    let inst = env.mgr.create(&slack("Work", &env)).unwrap().instance;
    let mut d = inst.to_draft();
    d.name = "Work Team".into();
    d.autostart = true;
    d.badge = Some(Badge { color: "#e01b24".into(), letter: Some("W".into()) });
    let updated = env.mgr.update(&inst.key(), &d).unwrap().instance;
    assert_eq!(updated.created, inst.created);
    let paths = env.mgr.paths();
    let autostart = paths.autostart_file("com.slack.Slack", "work");
    assert!(autostart.is_file());
    let badge = paths.badge_icon("com.slack.Slack", "work");
    assert!(badge.is_file());
    let kf = KeyFile::parse(&fs::read_to_string(paths.desktop_file("com.slack.Slack", "work")).unwrap());
    assert_eq!(kf.get("Desktop Entry", "Name"), Some("Slack (Work Team)"));
    assert_eq!(kf.get("Desktop Entry", "Icon"), Some(badge.to_str().unwrap()));

    d.autostart = false;
    d.badge = None;
    env.mgr.update(&inst.key(), &d).unwrap();
    assert!(!autostart.exists());
    assert!(!badge.exists());

    let mut bad = d.clone();
    bad.slug = "other".into();
    assert!(env.mgr.update(&inst.key(), &bad).is_err());

    let dup = env.mgr.duplicate(&inst.key(), "Work").unwrap().instance;
    assert_eq!(dup.slug, "work-2");
    assert_eq!(dup.wm_class, "multipak-slack-work-2");
    assert_eq!(dup.data_dir, None);
}

#[test]
fn doctor_finds_and_fixes_problems() {
    let env = setup();
    let inst = env.mgr.create(&slack("Work", &env)).unwrap().instance;
    assert!(env.mgr.doctor().unwrap().is_empty(), "{:?}", env.mgr.doctor().unwrap());
    let paths = env.mgr.paths();
    let desktop = paths.desktop_file("com.slack.Slack", "work");

    fs::write(&desktop, "[Desktop Entry]\nName=old\n").unwrap();
    fs::remove_dir_all(inst.resolved_data_dir(paths)).unwrap();
    let orphan = paths.applications_dir().join("multipak-com.slack.Slack-gone.desktop");
    fs::write(&orphan, "[Desktop Entry]\nX-Multipak-App=com.slack.Slack\nX-Multipak-Instance=gone\n").unwrap();
    let unrelated = paths.applications_dir().join("multipak-notes.desktop");
    fs::write(&unrelated, "[Desktop Entry]\nName=Not ours\n").unwrap();

    let issues = env.mgr.doctor().unwrap();
    let kinds: Vec<_> = issues.iter().map(|i| i.kind.clone()).collect();
    assert!(kinds.contains(&IssueKind::LauncherOutdated));
    assert!(kinds.contains(&IssueKind::DataDirMissing));
    assert!(kinds.contains(&IssueKind::OrphanFile));
    assert_eq!(issues.len(), 3, "{issues:?}");
    for issue in &issues {
        env.mgr.apply_fix(issue.fix.as_ref().unwrap()).unwrap();
    }
    assert!(env.mgr.doctor().unwrap().is_empty());
    assert!(!orphan.exists());
    assert!(unrelated.exists());

    // Fixes refuse to delete files Multipak did not create.
    assert!(env.mgr.apply_fix(&Fix::RemoveFile(env.home.join(".bashrc"))).is_err());

    // Uninstalled app.
    env.host.on("flatpak", &["info", "--show-ref"], CmdOutput::failed(1, "not installed"));
    let issues = env.mgr.doctor().unwrap();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].kind, IssueKind::AppNotInstalled);
    env.mgr.apply_fix(issues[0].fix.as_ref().unwrap()).unwrap();
    assert!(env.mgr.load().unwrap().instances.is_empty());
    assert!(inst.resolved_data_dir(paths).exists(), "data kept");

    env.host.remove_executable("flatpak");
    assert_eq!(env.mgr.doctor().unwrap()[0].kind, IssueKind::FlatpakMissing);
}

#[test]
fn backup_restore_and_clear_cache() {
    let env = setup();
    let inst = env.mgr.create(&slack("Work", &env)).unwrap().instance;
    let data = inst.resolved_data_dir(env.mgr.paths());
    fs::create_dir_all(data.join("config/Slack/Cache")).unwrap();
    fs::write(data.join("config/Slack/Cache/blob"), vec![0u8; 50_000]).unwrap();
    fs::write(data.join("config/Slack/settings.json"), "{}").unwrap();
    let cancel = CancelToken::new();
    assert!(env.mgr.disk_usage(&inst.key(), &cancel).unwrap() >= 50_000);

    let archive = env.home.join("backup.tar.zst");
    env.mgr.backup(&inst.key(), &archive, &|_| {}, &cancel).unwrap();

    let report = env.mgr.clear_cache(&inst.key(), &cancel).unwrap();
    assert_eq!(report.dirs, vec![data.join("cache"), data.join("config/Slack/Cache")]);
    assert!(!data.join("config/Slack/Cache/blob").exists());
    assert!(data.join("config/Slack/settings.json").exists());

    env.mgr.restore(&archive, RestoreTarget::Existing(inst.key()), &|_| {}, &cancel).unwrap();
    assert!(data.join("config/Slack/Cache/blob").exists());

    let copy =
        env.mgr.restore(&archive, RestoreTarget::New { slug: Some("work-copy".into()) }, &|_| {}, &cancel).unwrap();
    assert_eq!(copy.slug, "work-copy");
    assert_eq!(copy.name, "Work (copy)");
    assert!(copy.resolved_data_dir(env.mgr.paths()).join("config/Slack/settings.json").exists());
    assert!(matches!(
        env.mgr.restore(&archive, RestoreTarget::New { slug: None }, &|_| {}, &cancel),
        Err(Error::DuplicateSlug { .. })
    ));
}

#[test]
fn export_and_import_to_another_machine() {
    let env = setup();
    let inst = env.mgr.create(&slack("Work", &env)).unwrap().instance;
    fs::write(inst.resolved_data_dir(env.mgr.paths()).join("token"), "secret").unwrap();
    let mut d = slack("Client", &env);
    d.data_dir = Some(env.home.join("Clients/one"));
    env.mgr.create(&d).unwrap();

    let export = env.home.join("exports/setup.json");
    let report = env.mgr.export(&export, true, &|_| {}, &CancelToken::new()).unwrap();
    assert_eq!(report.instances, 2);
    assert_eq!(report.backups.len(), 2);
    let text = fs::read_to_string(&export).unwrap();
    assert!(text.contains("\"~/Clients/one\""), "{text}");
    assert!(!text.contains(env.home.to_str().unwrap()), "export must be portable");

    // A second machine with a different home folder.
    let tmp2 = tempfile::tempdir().unwrap();
    let home2 = tmp2.path().join("other-user");
    fs::create_dir_all(&home2).unwrap();
    let env2 = env_with(tmp2, home2.clone());
    let moved = home2.join("setup.json");
    fs::copy(&export, &moved).unwrap();
    let data_dir = multipak_core::transfer::data_dir_for(&export);
    let target_data = multipak_core::transfer::data_dir_for(&moved);
    fs::create_dir_all(&target_data).unwrap();
    for e in fs::read_dir(&data_dir).unwrap().flatten() {
        fs::copy(e.path(), target_data.join(e.file_name())).unwrap();
    }

    // Both instances are created; custom folders need no extra permission.
    let r = env2
        .mgr
        .import(&moved, ImportOptions { restore_data: true, ..Default::default() }, &|_| {}, &CancelToken::new())
        .unwrap();
    let mut created = r.created.clone();
    created.sort();
    assert_eq!(created, vec![key("client"), key("work")]);
    let mut restored = r.restored.clone();
    restored.sort();
    assert_eq!(restored, vec![key("client"), key("work")]);
    assert!(r.skipped.is_empty());
    assert_eq!(fs::read_to_string(home2.join(".var/app/com.slack.Slack/instances/work/token")).unwrap(), "secret");

    let r = env2
        .mgr
        .import(
            &moved,
            ImportOptions { on_conflict: OnConflict::Rename, restore_data: false },
            &|_| {},
            &CancelToken::new(),
        )
        .unwrap();
    let mut created = r.created.clone();
    created.sort();
    // A renamed copy of `client` would share its custom data folder, so it is skipped.
    assert_eq!(created, vec![key("work-2")]);
    assert_eq!(r.skipped.len(), 1);
    let client = env2.mgr.load().unwrap().find("com.slack.Slack/client").unwrap().clone();
    assert_eq!(client.data_dir.as_deref(), Some(home2.join("Clients/one").as_path()));
    assert_eq!(client.managed_override, None);
}

#[test]
fn hide_from_capture_installs_per_process_helper() {
    use multipak_core::capture;
    let env = setup();
    env.host.add_executable("gdbus", "/usr/bin/gdbus");
    let paths = env.mgr.paths().clone();
    let exe = std::path::PathBuf::from("/usr/bin/multipak");
    let rules = capture::rules_file(&paths);
    fs::create_dir_all(rules.parent().unwrap()).unwrap();
    // A class rule left by the first version is cleaned up.
    let user_rules = "[1]\nDescription=Mine\nwmclass=foo\n\n[General]\nOrder=1\n";
    fs::write(&rules, user_rules).unwrap();
    capture::set_rule(&paths, "multipak-com.slack.Slack-old", "x", "x", true).unwrap();

    let mut d = slack("Work", &env);
    d.hide_from_capture = true;
    let inst = env.mgr.create(&d).unwrap().instance;
    assert!(capture::helper_installed(&paths, &exe), "script, service file and plugin switch");
    assert_eq!(fs::read_to_string(&rules).unwrap(), user_rules);
    assert!(env.mgr.doctor().unwrap().is_empty(), "{:?}", env.mgr.doctor().unwrap());

    // The helper answers per process, so two instances of the same app are told apart.
    let other = env.mgr.create(&slack("Home", &env)).unwrap().instance;
    env.host.add_process(
        500,
        FakeProcess { environ: vec![format!("MULTIPAK_INSTANCE={}", inst.key())], ..Default::default() },
    );
    env.host.add_process(501, FakeProcess { parent: Some(500), ..Default::default() });
    env.host.add_process(
        600,
        FakeProcess { environ: vec![format!("MULTIPAK_INSTANCE={}", other.key())], ..Default::default() },
    );
    env.host.add_process(700, FakeProcess { environ: vec!["FLATPAK_ID=com.slack.Slack".into()], ..Default::default() });
    let config = env.mgr.load().unwrap();
    assert_eq!(capture::decide(env.mgr.host(), &config, 501), capture::DECISION_HIDE);
    assert_eq!(capture::decide(env.mgr.host(), &config, 600), capture::DECISION_SHOW);
    assert_eq!(capture::decide(env.mgr.host(), &config, 700), capture::DECISION_NOT_OURS);

    // The doctor notices a broken install, and a moved executable.
    fs::remove_file(capture::service_file(&paths)).unwrap();
    let issues = env.mgr.doctor().unwrap();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].kind, IssueKind::CaptureHelperMissing);
    env.mgr.apply_fix(issues[0].fix.as_ref().unwrap()).unwrap();
    assert!(env.mgr.doctor().unwrap().is_empty());

    // Turning the last one off removes everything again.
    let mut off = inst.to_draft();
    off.hide_from_capture = false;
    env.mgr.update(&inst.key(), &off).unwrap();
    assert!(!capture::helper_present(&paths));

    let mut on = inst.to_draft();
    on.hide_from_capture = true;
    env.mgr.update(&inst.key(), &on).unwrap();
    assert!(capture::helper_installed(&paths, &exe));
    env.mgr.delete(&inst.key(), false).unwrap();
    assert!(!capture::helper_present(&paths));

    // Leftover class rules from older versions are reported.
    capture::set_rule(&paths, "multipak-com.slack.Slack-gone", "x", "x", true).unwrap();
    let issues = env.mgr.doctor().unwrap();
    assert_eq!(issues[0].kind, IssueKind::OrphanCaptureRule);
    env.mgr.apply_fix(issues[0].fix.as_ref().unwrap()).unwrap();
    assert_eq!(fs::read_to_string(&rules).unwrap(), user_rules);

    // Multipak's own window still uses a class rule (its class is reliable).
    env.mgr.set_self_hidden_from_capture(true).unwrap();
    assert!(env.mgr.self_hidden_from_capture());
    env.mgr.set_self_hidden_from_capture(false).unwrap();
    assert_eq!(fs::read_to_string(&rules).unwrap(), user_rules);
}

#[test]
fn hide_from_capture_warns_on_unsupported_desktops() {
    let mut env = setup();
    env.mgr.set_capture_support(multipak_core::capture::Support::Unsupported("GNOME".into()));
    let mut d = slack("Work", &env);
    d.hide_from_capture = true;
    let out = env.mgr.create(&d).unwrap();
    assert!(out.warnings.iter().any(|w| w.contains("GNOME")), "{:?}", out.warnings);
}

#[test]
fn concurrent_creates_do_not_lose_instances() {
    let env = setup();
    let mgr = Arc::new(env.mgr);
    let handles: Vec<_> = (0..6)
        .map(|n| {
            let mgr = mgr.clone();
            std::thread::spawn(move || {
                let mut d =
                    InstanceDraft::new("com.slack.Slack", "Slack", &format!("Account {n}"), Strategy::Electron, &[]);
                d.slug = format!("account-{n}");
                d.wm_class = format!("multipak-slack-account-{n}");
                mgr.create(&d).unwrap();
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(mgr.load().unwrap().instances.len(), 6);
}
