//! Multipak: create and run multiple isolated instances of the same Flatpak app.
//!
//! `multipak` with no arguments opens the GUI; subcommands give the same features on the
//! command line (see `multipak --help`).

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
    let manager = Arc::new(Manager::with_types(Arc::new(SystemHost), paths, types));
    match args.command {
        None | Some(cli::Command::Gui) => gui::run(manager, warnings).map(|()| ExitCode::SUCCESS),
        Some(cmd) => cli::run(cmd, &manager),
    }
}
