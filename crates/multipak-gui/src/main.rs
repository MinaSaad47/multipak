//! Multipak: create and run multiple isolated instances of the same Flatpak app.
//!
//! `multipak` with no arguments opens the GUI; subcommands give the same features on the
//! command line (see `multipak --help`).

mod capture_service;
mod cli;
mod gui;
mod logging;

use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser as _;
use multipak_core::{AppTypeTable, Manager, Paths, SystemHost};

fn main() -> ExitCode {
    let args = cli::Cli::parse();
    match real_main(args) {
        Ok(code) => code,
        Err(e) => {
            // Only to the log file (if enabled); stderr gets the plain message below.
            tracing::debug!("exiting with error: {e:#}");
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// The executable D-Bus should start for the capture helper: the AppImage file itself when
/// running from an AppImage (its mount point changes on every start), otherwise this binary.
fn helper_executable() -> Option<std::path::PathBuf> {
    std::env::var_os("APPIMAGE")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::current_exe().ok())
}

fn real_main(args: cli::Cli) -> anyhow::Result<ExitCode> {
    let paths = Paths::from_env()?;
    let gui_mode = matches!(args.command, None | Some(cli::Command::Gui));
    let log_file = args.log_file || std::env::var_os("MULTIPAK_LOG_FILE").is_some_and(|v| v != "0" && !v.is_empty());
    if let Some(path) = logging::init(args.verbose, log_file, gui_mode, &paths)? {
        tracing::info!("logging to {}", path.display());
    }
    tracing::debug!("multipak {} starting", env!("CARGO_PKG_VERSION"));

    let mut warnings = Vec::new();
    let types = match AppTypeTable::load(&paths) {
        Ok(t) => t,
        Err(e) => {
            let msg = format!("{e}. Using the built-in app table only.");
            if gui_mode {
                warnings.push(msg);
            } else {
                eprintln!("warning: {msg}");
            }
            AppTypeTable::builtin()
        }
    };
    let mut manager = Manager::with_types(Arc::new(SystemHost), paths, types);
    if let Some(exe) = helper_executable() {
        manager.set_capture_helper(exe);
    }
    let manager = Arc::new(manager);
    match args.command {
        None | Some(cli::Command::Gui) => gui::run(manager, warnings).map(|()| ExitCode::SUCCESS),
        Some(cli::Command::CaptureService) => capture_service::run(manager).map(|()| ExitCode::SUCCESS),
        Some(cmd) => cli::run(cmd, &manager),
    }
}
