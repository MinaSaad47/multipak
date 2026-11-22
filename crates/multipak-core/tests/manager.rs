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
    let mgr = Manager::new(Arc::new(host.clone()), paths).unwrap();
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
    let out = env.mgr.create(&slack("Work", &env), false).unwrap();
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
    assert_eq!(exec[0], "/usr/bin/flatpak");
    assert!(exec.contains(&format!("--env=XDG_CONFIG_HOME={}", data.display())));
    assert!(!kf.get(g, "Exec").unwrap().contains("$HOME"));
    assert_eq!(
        env.host.calls_matching(&[&paths.applications_dir().display().to_string()]).len(),
        1,
        "update-desktop-database runs"
    );
    let cfg = env.mgr.load().unwrap();
    assert_eq!(cfg.instances.len(), 1);
    assert!(matches!(env.mgr.create(&slack("x", &env).clone_with_slug("work"), false), Err(Error::Invalid { .. })));
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
    env.mgr.create(&slack("Work", &env), false).unwrap();
    assert!(env.host.calls().iter().all(|c| !c.program.contains("update-desktop-database")));
}

#[test]
fn custom_dir_requires_consent_and_is_cleaned_up() {
    let env = setup();
    let custom = env.home.join("Clients/acme");
    let mut d = slack("Acme", &env);
    d.data_dir = Some(custom.clone());
    let err = env.mgr.create(&d, false).unwrap_err();
    assert!(matches!(err, Error::OverrideRequired { .. }), "{err}");
    assert!(env.mgr.load().unwrap().instances.is_empty());
    assert!(!env.mgr.paths().desktop_file("com.slack.Slack", "acme").exists());

    let inst = env.mgr.create(&d, true).unwrap().instance;
    assert_eq!(inst.managed_override.as_deref(), Some(custom.to_str().unwrap()));
    let overrides = env.mgr.paths().overrides_file("com.slack.Slack");
    assert!(fs::read_to_string(&overrides).unwrap().contains(custom.to_str().unwrap()));

    let report = env.mgr.delete(&inst.key(), false).unwrap();
    assert_eq!(report.removed_override.as_deref(), custom.to_str());
    assert!(!fs::read_to_string(&overrides).unwrap().contains(custom.to_str().unwrap()));
    assert!(custom.is_dir(), "data is kept by default");
}

#[test]
fn existing_override_is_not_claimed() {
    let env = setup();
    let custom = env.home.join("Shared/slack");
    let overrides = env.mgr.paths().overrides_file("com.slack.Slack");
    fs::create_dir_all(overrides.parent().unwrap()).unwrap();
    fs::write(&overrides, format!("[Context]\nfilesystems={};\n", custom.display())).unwrap();
    let mut d = slack("Shared", &env);
    d.data_dir = Some(custom.clone());
    let inst = env.mgr.create(&d, false).unwrap().instance;
    assert_eq!(inst.managed_override, None);
    env.mgr.delete(&inst.key(), false).unwrap();
    assert!(fs::read_to_string(&overrides).unwrap().contains(custom.to_str().unwrap()), "user's override untouched");
}

#[test]
fn delete_data_is_guarded() {
    let env = setup();
    let inst = env.mgr.create(&slack("Work", &env), false).unwrap().instance;
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
    let inst = env.mgr.create(&slack("Work", &env), false).unwrap().instance;
    assert!(matches!(env.mgr.launch(&inst.key()).unwrap(), LaunchOutcome::Running { .. }));
    let spec = env.host.launches().pop().unwrap();
    assert_eq!(spec.program, "/usr/bin/flatpak");
    assert_eq!(spec.args[1], "--env=MULTIPAK_INSTANCE=com.slack.Slack/work");
    assert!(spec.args.contains(&"--class=multipak-slack-work".to_string()));

    env.host.set_launch_outcome(LaunchOutcome::Failed { code: Some(1), log_tail: "error: app not installed".into() });
    let err = env.mgr.launch(&inst.key()).unwrap_err();
    assert!(err.to_string().contains("app not installed"), "{err}");
    assert!(matches!(env.mgr.launch(&key("nope")), Err(Error::InstanceNotFound(_))));
}

#[test]
fn firefox_profile_is_created_once() {
    let env = setup();
    let d = InstanceDraft::new("org.mozilla.firefox", "Firefox", "Client A", Strategy::Firefox, &[]);
    let inst = env.mgr.create(&d, false).unwrap().instance;
    let calls = env.host.calls_matching(&["run", "org.mozilla.firefox", "-CreateProfile"]);
    assert_eq!(calls.len(), 1);
    let dir = inst.resolved_data_dir(env.mgr.paths());
    assert_eq!(calls[0].args[3], format!("multipak-client-a {}", dir.display()));
    fs::write(dir.join("times.json"), "{}").unwrap();
    env.mgr.launch(&inst.key()).unwrap();
    assert_eq!(env.host.calls_matching(&["run", "org.mozilla.firefox", "-CreateProfile"]).len(), 1);
    let spec = env.host.launches().pop().unwrap();
    let pos = spec.args.iter().position(|a| a == "-profile").unwrap();
    assert_eq!(spec.args[pos + 1], dir.display().to_string());
    assert!(spec.args.contains(&"--new-instance".to_string()));
}

#[test]
fn running_detection_and_stop() {
    let env = setup();
    let work = env.mgr.create(&slack("Work", &env), false).unwrap().instance;
    let home = env.mgr.create(&slack("Home", &env), false).unwrap().instance;
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
    // Sandbox 222: environment unreadable, identified by its --class argument.
    env.host.add_process(
        21,
        FakeProcess { cmdline: vec!["/app/slack".into(), format!("--class={}", home.wm_class)], ..Default::default() },
    );
    // Sandbox 333: the app's normal launcher.
    env.host.add_process(31, FakeProcess { cmdline: vec!["/app/slack".into()], ..Default::default() });

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
    let inst = env.mgr.create(&slack("Work", &env), false).unwrap().instance;
    let mut d = inst.to_draft();
    d.name = "Work Team".into();
    d.autostart = true;
    d.badge = Some(Badge { color: "#e01b24".into(), letter: Some("W".into()) });
    let updated = env.mgr.update(&inst.key(), &d, false).unwrap().instance;
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
    env.mgr.update(&inst.key(), &d, false).unwrap();
    assert!(!autostart.exists());
    assert!(!badge.exists());

    let mut bad = d.clone();
    bad.slug = "other".into();
    assert!(env.mgr.update(&inst.key(), &bad, false).is_err());

    let dup = env.mgr.duplicate(&inst.key(), "Work").unwrap().instance;
    assert_eq!(dup.slug, "work-2");
    assert_eq!(dup.wm_class, "multipak-slack-work-2");
    assert_eq!(dup.data_dir, None);
}

#[test]
fn doctor_finds_and_fixes_problems() {
    let env = setup();
    let inst = env.mgr.create(&slack("Work", &env), false).unwrap().instance;
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
    let inst = env.mgr.create(&slack("Work", &env), false).unwrap().instance;
    let data = inst.resolved_data_dir(env.mgr.paths());
    fs::create_dir_all(data.join("Slack/Cache")).unwrap();
    fs::write(data.join("Slack/Cache/blob"), vec![0u8; 50_000]).unwrap();
    fs::write(data.join("Slack/settings.json"), "{}").unwrap();
    let cancel = CancelToken::new();
    assert!(env.mgr.disk_usage(&inst.key(), &cancel).unwrap() >= 50_000);

    let archive = env.home.join("backup.tar.zst");
    env.mgr.backup(&inst.key(), &archive, &|_| {}, &cancel).unwrap();

    let report = env.mgr.clear_cache(&inst.key(), &cancel).unwrap();
    assert_eq!(report.dirs, vec![data.join("Slack/Cache")]);
    assert!(!data.join("Slack/Cache/blob").exists());
    assert!(data.join("Slack/settings.json").exists());

    env.mgr.restore(&archive, RestoreTarget::Existing(inst.key()), &|_| {}, &cancel).unwrap();
    assert!(data.join("Slack/Cache/blob").exists());

    let copy = env
        .mgr
        .restore(
            &archive,
            RestoreTarget::New { slug: Some("work-copy".into()), allow_override: false },
            &|_| {},
            &cancel,
        )
        .unwrap();
    assert_eq!(copy.slug, "work-copy");
    assert_eq!(copy.name, "Work (copy)");
    assert!(copy.resolved_data_dir(env.mgr.paths()).join("Slack/settings.json").exists());
    assert!(matches!(
        env.mgr.restore(&archive, RestoreTarget::New { slug: None, allow_override: false }, &|_| {}, &cancel),
        Err(Error::DuplicateSlug { .. })
    ));
}

#[test]
fn export_and_import_to_another_machine() {
    let env = setup();
    let inst = env.mgr.create(&slack("Work", &env), false).unwrap().instance;
    fs::write(inst.resolved_data_dir(env.mgr.paths()).join("token"), "secret").unwrap();
    let mut d = slack("Client", &env);
    d.data_dir = Some(env.home.join("Clients/one"));
    env.mgr.create(&d, true).unwrap();

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

    // Without consent the custom-folder instance is skipped, the other is created.
    let r = env2
        .mgr
        .import(&moved, ImportOptions { restore_data: true, ..Default::default() }, &|_| {}, &CancelToken::new())
        .unwrap();
    assert_eq!(r.created, vec![key("work")]);
    assert_eq!(r.restored, vec![key("work")]);
    assert_eq!(r.skipped.len(), 1);
    assert_eq!(fs::read_to_string(home2.join(".var/app/com.slack.Slack/instances/work/token")).unwrap(), "secret");

    let r = env2
        .mgr
        .import(
            &moved,
            ImportOptions { on_conflict: OnConflict::Rename, allow_overrides: true, restore_data: false },
            &|_| {},
            &CancelToken::new(),
        )
        .unwrap();
    let mut created = r.created.clone();
    created.sort();
    assert_eq!(created, vec![key("client"), key("work-2")]);
    let client = env2.mgr.load().unwrap().find("com.slack.Slack/client").unwrap().clone();
    assert_eq!(client.data_dir.as_deref(), Some(home2.join("Clients/one").as_path()));
    assert!(client.managed_override.is_some());
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
                mgr.create(&d, false).unwrap();
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(mgr.load().unwrap().instances.len(), 6);
}
