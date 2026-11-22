//! Command-line interface. Uses the same `multipak-core` operations as the GUI.

use std::collections::BTreeMap;
use std::io::{BufRead as _, IsTerminal as _, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context as _, bail};
use clap::{Args, Parser, Subcommand};
use multipak_core::backup::{self, CancelToken, Progress};
use multipak_core::instance::{self, InstanceKey};
use multipak_core::manager::RestoreTarget;
use multipak_core::transfer::{ImportOptions, OnConflict};
use multipak_core::{Badge, Error, Fix, InstanceDraft, LaunchOutcome, Manager, Strategy};

#[derive(Debug, Parser)]
#[command(
    name = "multipak",
    version,
    about = "Run several isolated instances of the same Flatpak app, each with its own data folder.",
    long_about = "Run several isolated instances of the same Flatpak app, each with its own data folder.\n\nWithout a command, the graphical interface opens.\n\nINSTANCE arguments accept <app-id>/<short-name>, a short name, or an instance name, as long as it is unambiguous."
)]
pub struct Cli {
    /// Show debug output.
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Also write logs to ~/.local/state/multipak/multipak.log (or set MULTIPAK_LOG_FILE=1).
    #[arg(long, global = true)]
    pub log_file: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Open the graphical interface (the default).
    Gui,
    /// List instances.
    List {
        /// Only instances of this app.
        #[arg(long)]
        app: Option<String>,
        /// Print JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// List installed Flatpak apps.
    Apps {
        /// Also detect each app's launch type (slower).
        #[arg(long)]
        detect: bool,
        /// Print JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Create an instance.
    Create(CreateArgs),
    /// Start an instance in the background.
    Launch { instance: String },
    /// Stop a running instance.
    Stop { instance: String },
    /// Delete an instance. Its data folder is kept unless --delete-data is given.
    Delete {
        instance: String,
        /// Also delete the data folder. Asks you to type the short name unless --confirm is given.
        #[arg(long)]
        delete_data: bool,
        /// The instance's short name, to confirm --delete-data without a prompt.
        #[arg(long, value_name = "SHORT_NAME", requires = "delete_data")]
        confirm: Option<String>,
    },
    /// Back up an instance's data to a .tar.zst file.
    Backup {
        instance: String,
        /// Output file (default: <app-id>-<short-name>-<date>.tar.zst in the current folder).
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Restore a backup. By default it goes back into the instance it came from
    /// (which is recreated if it no longer exists).
    Restore {
        file: PathBuf,
        /// Restore into this existing instance instead.
        #[arg(long, value_name = "INSTANCE", conflicts_with = "as_slug")]
        into: Option<String>,
        /// Create a new instance with this short name.
        #[arg(long = "as", value_name = "SHORT_NAME")]
        as_slug: Option<String>,
        /// Allow `flatpak override` if the backup uses a custom data folder.
        #[arg(long)]
        grant_access: bool,
    },
    /// Hide windows from screenshots, recordings and screen sharing (KDE Plasma).
    Capture {
        /// `on` to hide, `off` to show again.
        #[arg(value_parser = ["on", "off"])]
        state: String,
        /// The instance to change.
        #[arg(required_unless_present = "self_window", conflicts_with = "self_window")]
        instance: Option<String>,
        /// Change Multipak's own window instead of an instance.
        #[arg(long = "self")]
        self_window: bool,
    },
    /// D-Bus helper used by the KWin capture script (started automatically by D-Bus).
    #[command(hide = true)]
    CaptureService,
    /// Check launchers, folders and installed apps.
    Doctor {
        /// Apply the available fixes (instances of uninstalled apps are only removed with --remove-missing).
        #[arg(long)]
        fix: bool,
        /// With --fix, also remove instances whose app is no longer installed (their data is kept).
        #[arg(long, requires = "fix")]
        remove_missing: bool,
    },
    /// Export the instance setup as JSON.
    Export {
        file: PathBuf,
        /// Also write data backups into a folder next to the file.
        #[arg(long)]
        with_data: bool,
    },
    /// Import instances from an exported JSON file.
    Import {
        file: PathBuf,
        /// Import instances whose short name already exists under a new short name instead of skipping them.
        #[arg(long)]
        rename: bool,
        /// Allow `flatpak override` for instances with custom data folders.
        #[arg(long)]
        grant_access: bool,
        /// Do not restore included data backups.
        #[arg(long)]
        no_data: bool,
    },
}

#[derive(Debug, Args)]
pub struct CreateArgs {
    /// Flatpak app ID, e.g. com.slack.Slack.
    pub app_id: String,
    /// Instance name, e.g. "Work".
    pub name: String,
    /// Short name used in file names (default: generated from the name).
    #[arg(long)]
    pub slug: Option<String>,
    /// Window class (default: multipak-<app>-<short-name>).
    #[arg(long = "class", value_name = "WM_CLASS")]
    pub wm_class: Option<String>,
    /// Launch type: electron, chromium, firefox or generic (default: detected).
    #[arg(long)]
    pub strategy: Option<Strategy>,
    /// Custom data folder (default: ~/.var/app/<app-id>/instances/<short-name>).
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
    /// Allow `flatpak override` when the data folder is outside the app's sandbox.
    #[arg(long)]
    pub grant_access: bool,
    /// Extra argument for the app (repeat for several; use --arg=--flag for flags).
    #[arg(long = "arg", value_name = "ARG", allow_hyphen_values = true)]
    pub args: Vec<String>,
    /// Environment variable KEY=VALUE (repeatable).
    #[arg(long = "env", value_name = "KEY=VALUE")]
    pub env: Vec<String>,
    /// Badge color for the launcher icon, e.g. "#3584e4".
    #[arg(long)]
    pub badge_color: Option<String>,
    /// Letter shown in the badge.
    #[arg(long, requires = "badge_color")]
    pub badge_letter: Option<String>,
    /// Start this instance when you log in.
    #[arg(long)]
    pub autostart: bool,
    /// Hide this instance from screenshots, recordings and screen sharing (KDE Plasma).
    #[arg(long)]
    pub hide_from_capture: bool,
}

/// Runs one CLI command.
pub fn run(command: Command, mgr: &Manager) -> anyhow::Result<ExitCode> {
    match command {
        Command::Gui | Command::CaptureService => unreachable!("handled by main"),
        Command::List { app, json } => list(mgr, app.as_deref(), json),
        Command::Apps { detect, json } => apps(mgr, detect, json),
        Command::Create(args) => create(mgr, args),
        Command::Launch { instance } => {
            let key = resolve(mgr, &instance)?;
            match mgr.launch(&key)? {
                LaunchOutcome::Running { pid } => println!("Started {key} (pid {pid})."),
                LaunchOutcome::ExitedOk => println!(
                    "{key} exited right away without an error. If the app was already open, it may have opened a window in the running copy."
                ),
                LaunchOutcome::Failed { .. } => unreachable!("Manager::launch turns failures into errors"),
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Stop { instance } => {
            let key = resolve(mgr, &instance)?;
            match mgr.stop(&key)? {
                0 => println!("{key} is not running."),
                n => println!("Stopped {key} ({n} sandbox{}).", if n == 1 { "" } else { "es" }),
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Delete { instance, delete_data, confirm } => delete(mgr, &instance, delete_data, confirm),
        Command::Backup { instance, output } => {
            let key = resolve(mgr, &instance)?;
            let config = mgr.load()?;
            let inst = config.get(&key).context("instance disappeared")?;
            let out = output.unwrap_or_else(|| PathBuf::from(backup::default_file_name(inst)));
            if mgr.is_running(&key).unwrap_or(false) {
                eprintln!("Note: {key} is running; files it changes during the backup may be inconsistent.");
            }
            let cancel = cancel_on_ctrl_c();
            let reporter = ProgressPrinter::new("Backing up");
            let m = mgr.backup(&key, &out, &|p| reporter.update(&p), &cancel);
            reporter.finish();
            let m = m?;
            println!("Backed up {key} ({}) to {}", human_bytes(m.total_bytes), out.display());
            Ok(ExitCode::SUCCESS)
        }
        Command::Restore { file, into, as_slug, grant_access } => {
            let manifest = backup::read_manifest(&file)?;
            let target = match (into, as_slug) {
                (Some(q), _) => RestoreTarget::Existing(resolve(mgr, &q)?),
                (None, Some(slug)) => RestoreTarget::New { slug: Some(slug), allow_override: grant_access },
                (None, None) => {
                    let key = manifest.instance.key();
                    if mgr.load()?.get(&key).is_some() {
                        RestoreTarget::Existing(key)
                    } else {
                        RestoreTarget::New { slug: None, allow_override: grant_access }
                    }
                }
            };
            let cancel = cancel_on_ctrl_c();
            let reporter = ProgressPrinter::new("Restoring");
            let r = mgr.restore(&file, target, &|p| reporter.update(&p), &cancel);
            reporter.finish();
            let inst = r?;
            println!("Restored {} into {}", inst.key(), inst.resolved_data_dir(mgr.paths()).display());
            Ok(ExitCode::SUCCESS)
        }
        Command::Doctor { fix, remove_missing } => doctor(mgr, fix, remove_missing),
        Command::Capture { state, instance, self_window } => {
            capture(mgr, state == "on", instance.as_deref(), self_window)
        }
        Command::Export { file, with_data } => {
            let cancel = cancel_on_ctrl_c();
            let reporter = ProgressPrinter::new("Exporting");
            let r = mgr.export(&file, with_data, &|p| reporter.update(&p), &cancel);
            reporter.finish();
            let report = r?;
            println!("Exported {} instance(s) to {}", report.instances, file.display());
            for b in &report.backups {
                println!("  data: {}", b.display());
            }
            for w in &report.warnings {
                eprintln!("warning: {w}");
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Import { file, rename, grant_access, no_data } => {
            let opts = ImportOptions {
                on_conflict: if rename { OnConflict::Rename } else { OnConflict::Skip },
                allow_overrides: grant_access,
                restore_data: !no_data,
            };
            let cancel = cancel_on_ctrl_c();
            let reporter = ProgressPrinter::new("Restoring data");
            let r = mgr.import(&file, opts, &|p| reporter.update(&p), &cancel);
            reporter.finish();
            let report = r?;
            for k in &report.created {
                let restored = if report.restored.contains(k) { " (data restored)" } else { "" };
                println!("Created {k}{restored}");
            }
            for (k, why) in &report.skipped {
                println!("Skipped {k}: {why}");
            }
            for w in &report.warnings {
                eprintln!("warning: {w}");
            }
            Ok(if report.skipped.is_empty() { ExitCode::SUCCESS } else { ExitCode::from(2) })
        }
    }
}

fn resolve(mgr: &Manager, query: &str) -> anyhow::Result<InstanceKey> {
    Ok(mgr.load()?.find(query)?.key())
}

fn list(mgr: &Manager, app: Option<&str>, json: bool) -> anyhow::Result<ExitCode> {
    let config = mgr.load()?;
    let running = mgr.running().ok();
    let instances: Vec<_> = config.instances.iter().filter(|i| app.is_none_or(|a| i.app_id == a)).collect();
    let is_running = |k: &InstanceKey| running.as_ref().map(|r| r.iter().any(|x| x.key.as_ref() == Some(k)));
    if json {
        let rows: Vec<_> = instances
            .iter()
            .map(|i| {
                serde_json::json!({
                    "key": i.key().to_string(),
                    "app_id": i.app_id,
                    "name": i.name,
                    "slug": i.slug,
                    "wm_class": i.wm_class,
                    "strategy": i.strategy,
                    "data_dir": i.resolved_data_dir(mgr.paths()),
                    "autostart": i.autostart,
                    "running": is_running(&i.key()),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(ExitCode::SUCCESS);
    }
    if instances.is_empty() {
        println!("No instances yet. Create one with `multipak create <app-id> <name>` or in the GUI.");
        return Ok(ExitCode::SUCCESS);
    }
    let rows: Vec<[String; 5]> = instances
        .iter()
        .map(|i| {
            let status = match is_running(&i.key()) {
                Some(true) => "running",
                Some(false) => "stopped",
                None => "?",
            };
            [
                i.key().to_string(),
                i.name.clone(),
                i.strategy.to_string(),
                status.to_string(),
                mgr.paths().tildify(&i.resolved_data_dir(mgr.paths())),
            ]
        })
        .collect();
    print_table(&["INSTANCE", "NAME", "TYPE", "STATUS", "DATA FOLDER"], &rows);
    Ok(ExitCode::SUCCESS)
}

fn apps(mgr: &Manager, detect: bool, json: bool) -> anyhow::Result<ExitCode> {
    let apps = mgr.list_apps()?;
    let config = mgr.load()?;
    let mut rows = Vec::new();
    let mut json_rows = Vec::new();
    for a in &apps {
        let count = config.for_app(&a.app_id).count();
        let kind = detect.then(|| mgr.app_details(&a.app_id).detection.strategy.to_string());
        json_rows.push(serde_json::json!({
            "app_id": a.app_id, "name": a.name, "origin": a.origin,
            "installation": a.installation, "instances": count, "type": kind,
        }));
        let mut row = vec![a.app_id.clone(), a.name.clone(), a.installation.clone(), count.to_string()];
        if let Some(k) = kind {
            row.push(k);
        }
        rows.push(row);
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&json_rows)?);
    } else if detect {
        let rows: Vec<[String; 5]> =
            rows.into_iter().map(|r| [r[0].clone(), r[1].clone(), r[2].clone(), r[3].clone(), r[4].clone()]).collect();
        print_table(&["APP ID", "NAME", "INSTALLATION", "INSTANCES", "TYPE"], &rows);
    } else {
        let rows: Vec<[String; 4]> =
            rows.into_iter().map(|r| [r[0].clone(), r[1].clone(), r[2].clone(), r[3].clone()]).collect();
        print_table(&["APP ID", "NAME", "INSTALLATION", "INSTANCES"], &rows);
    }
    Ok(ExitCode::SUCCESS)
}

fn create(mgr: &Manager, a: CreateArgs) -> anyhow::Result<ExitCode> {
    let installed = mgr.list_apps()?;
    let app =
        installed.iter().find(|x| x.app_id == a.app_id).ok_or_else(|| Error::AppNotInstalled(a.app_id.clone()))?;
    let details = mgr.app_details(&a.app_id);
    let config = mgr.load()?;
    let strategy = a.strategy.unwrap_or(details.detection.strategy);
    let mut draft = InstanceDraft::new(&a.app_id, &app.name, &a.name, strategy, &config.instances);
    if let Some(slug) = a.slug {
        draft.wm_class = instance::default_wm_class(&a.app_id, &slug);
        draft.slug = slug;
    }
    if let Some(class) = a.wm_class {
        draft.wm_class = class;
    }
    draft.data_dir =
        a.data_dir.map(|d| if d.is_relative() { std::env::current_dir().map(|c| c.join(&d)).unwrap_or(d) } else { d });
    draft.extra_args = a.args;
    let mut env = BTreeMap::new();
    for pair in &a.env {
        let (k, v) = instance::parse_env_pair(pair)?;
        env.insert(k, v);
    }
    draft.env = env;
    draft.badge = a.badge_color.map(|color| Badge { color, letter: a.badge_letter });
    draft.autostart = a.autostart;
    draft.hide_from_capture = a.hide_from_capture;
    if let Some(w) = details.detection.single_instance.warning() {
        eprintln!("warning: {w}");
    }
    let outcome = match mgr.create(&draft, a.grant_access) {
        Err(e @ Error::OverrideRequired { .. }) => bail!("{e}\nRe-run with --grant-access to allow it."),
        other => other?,
    };
    for w in &outcome.warnings {
        eprintln!("warning: {w}");
    }
    let i = &outcome.instance;
    println!("Created {} ({})", i.key(), i.display_name());
    println!("  type:        {}", i.strategy.label());
    println!("  data folder: {}", i.resolved_data_dir(mgr.paths()).display());
    println!("  launcher:    {}", mgr.paths().desktop_file(&i.app_id, &i.slug).display());
    if let Some(o) = &i.managed_override {
        println!("  access:      flatpak override --user --filesystem={o} {}", i.app_id);
    }
    Ok(ExitCode::SUCCESS)
}

fn delete(mgr: &Manager, query: &str, delete_data: bool, confirm: Option<String>) -> anyhow::Result<ExitCode> {
    let key = resolve(mgr, query)?;
    if delete_data {
        let config = mgr.load()?;
        let dir = config.get(&key).map(|i| i.resolved_data_dir(mgr.paths())).unwrap_or_default();
        let confirmed = match confirm {
            Some(c) => c == key.slug,
            None if std::io::stdin().is_terminal() => {
                eprint!("This permanently deletes {}.\nType {} to confirm: ", dir.display(), key.slug);
                std::io::stderr().flush().ok();
                let mut line = String::new();
                std::io::stdin().lock().read_line(&mut line)?;
                line.trim() == key.slug
            }
            None => bail!("refusing to delete data without confirmation; pass --confirm {}", key.slug),
        };
        if !confirmed {
            bail!("confirmation did not match; nothing was deleted");
        }
    }
    let report = mgr.delete(&key, delete_data)?;
    println!("Deleted {key}.");
    for f in &report.removed_files {
        println!("  removed {}", f.display());
    }
    if let Some(o) = &report.removed_override {
        println!("  removed folder access to {o}");
    }
    if report.data_deleted {
        println!("  deleted data folder {}", report.data_dir.display());
    } else {
        println!("  kept data folder {}", report.data_dir.display());
    }
    Ok(ExitCode::SUCCESS)
}

fn capture(mgr: &Manager, hide: bool, instance: Option<&str>, self_window: bool) -> anyhow::Result<ExitCode> {
    let (what, warnings) = if self_window {
        ("Multipak".to_string(), mgr.set_self_hidden_from_capture(hide)?)
    } else {
        let query = instance.context("name an instance or pass --self")?;
        let key = resolve(mgr, query)?;
        let config = mgr.load()?;
        let inst = config.get(&key).context("instance disappeared")?;
        let mut draft = inst.to_draft();
        draft.hide_from_capture = hide;
        (inst.display_name(), mgr.update(&key, &draft, false)?.warnings)
    };
    for w in &warnings {
        eprintln!("warning: {w}");
    }
    let verb = if hide { "is now hidden from" } else { "is visible again in" };
    println!("{what} {verb} screenshots, recordings and screen sharing.");
    Ok(ExitCode::SUCCESS)
}

fn doctor(mgr: &Manager, fix: bool, remove_missing: bool) -> anyhow::Result<ExitCode> {
    let issues = mgr.doctor()?;
    if issues.is_empty() {
        println!("Everything looks good.");
        return Ok(ExitCode::SUCCESS);
    }
    let mut remaining = 0;
    for issue in &issues {
        println!("• {issue}");
        match (&issue.fix, fix) {
            (Some(Fix::RemoveInstance(_)), true) if !remove_missing => {
                println!("  skipped (use --remove-missing to remove this instance; its data is kept)");
                remaining += 1;
            }
            (Some(f), true) => match mgr.apply_fix(f) {
                Ok(()) => println!("  fixed: {}", f.label()),
                Err(e) => {
                    println!("  fix failed: {e}");
                    remaining += 1;
                }
            },
            (Some(f), false) => {
                println!("  fix available: {}", f.label());
                remaining += 1;
            }
            (None, _) => remaining += 1,
        }
    }
    if !fix && remaining > 0 {
        println!("\nRun `multipak doctor --fix` to apply the available fixes.");
    }
    Ok(if remaining == 0 { ExitCode::SUCCESS } else { ExitCode::from(2) })
}

fn print_table<const N: usize>(headers: &[&str; N], rows: &[[String; N]]) {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let line = |cells: Vec<&str>| {
        let parts: Vec<String> = cells
            .iter()
            .zip(&widths)
            .enumerate()
            .map(|(i, (c, w))| if i + 1 == N { c.to_string() } else { format!("{c:<w$}") })
            .collect();
        println!("{}", parts.join("  "));
    };
    line(headers.to_vec());
    for row in rows {
        line(row.iter().map(String::as_str).collect());
    }
}

fn human_bytes(b: u64) -> String {
    crate::gui::model::format_bytes(b)
}

/// Cancels long operations on Ctrl+C so partial files are cleaned up.
fn cancel_on_ctrl_c() -> CancelToken {
    let token = CancelToken::new();
    let t = token.clone();
    if let Err(e) = ctrlc::set_handler(move || {
        if t.is_cancelled() {
            std::process::exit(130);
        }
        eprintln!("\nCancelling… (press Ctrl+C again to quit immediately)");
        t.cancel();
    }) {
        tracing::warn!("could not install the Ctrl+C handler: {e}");
    }
    token
}

/// Single-line progress on stderr when it is a terminal.
struct ProgressPrinter {
    label: &'static str,
    tty: bool,
    last: std::sync::Mutex<Option<u32>>,
}

impl ProgressPrinter {
    fn new(label: &'static str) -> Self {
        Self { label, tty: std::io::stderr().is_terminal(), last: std::sync::Mutex::new(None) }
    }

    fn update(&self, p: &Progress) {
        if !self.tty || p.total == 0 {
            return;
        }
        let pct = (p.fraction() * 100.0) as u32;
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if *last == Some(pct) {
            return;
        }
        *last = Some(pct);
        eprint!("\r{}: {pct:>3}% ({} of {})   ", self.label, human_bytes(p.done), human_bytes(p.total));
        std::io::stderr().flush().ok();
    }

    fn finish(&self) {
        if self.tty && self.last.lock().map(|l| l.is_some()).unwrap_or(false) {
            eprintln!();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_create_with_hyphen_args() {
        let cli = Cli::try_parse_from([
            "multipak",
            "create",
            "com.slack.Slack",
            "Work",
            "--arg=--enable-features=X",
            "--arg",
            "--disable-gpu",
            "--env",
            "LANG=C",
            "--strategy",
            "electron",
            "--badge-color",
            "#3584e4",
            "--badge-letter",
            "W",
        ])
        .unwrap();
        let Some(Command::Create(a)) = cli.command else { panic!("expected create") };
        assert_eq!(a.args, vec!["--enable-features=X", "--disable-gpu"]);
        assert_eq!(a.strategy, Some(Strategy::Electron));
        assert_eq!(a.env, vec!["LANG=C"]);
    }

    #[test]
    fn confirm_requires_delete_data() {
        assert!(Cli::try_parse_from(["multipak", "delete", "work", "--confirm", "work"]).is_err());
        assert!(Cli::try_parse_from(["multipak", "delete", "work", "--delete-data", "--confirm", "work"]).is_ok());
        assert!(Cli::try_parse_from(["multipak", "restore", "f.tar.zst", "--into", "a", "--as", "b"]).is_err());
        assert!(Cli::try_parse_from(["multipak", "capture", "on", "work"]).is_ok());
        assert!(Cli::try_parse_from(["multipak", "capture", "off", "--self"]).is_ok());
        assert!(Cli::try_parse_from(["multipak", "capture", "on"]).is_err());
        assert!(Cli::try_parse_from(["multipak", "capture", "on", "work", "--self"]).is_err());
        assert!(Cli::try_parse_from(["multipak", "capture", "maybe", "work"]).is_err());
    }
}
