//! Logging setup: `tracing` to stderr, plus an optional log file in
//! `~/.local/state/multipak/multipak.log`.

use std::fs::OpenOptions;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::Context as _;
use multipak_core::Paths;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{Layer as _, fmt};

/// Default log levels. The CLI is quiet unless `--verbose`; the GUI logs informational
/// messages because nobody sees its stderr unless they started it from a terminal.
pub fn default_filter(verbose: bool, gui: bool) -> String {
    match (verbose, gui) {
        (true, _) => "info,multipak=debug,multipak_core=debug,usvg=error,resvg=error".into(),
        (false, true) => "warn,multipak=info,multipak_core=info,usvg=error,resvg=error".into(),
        (false, false) => "warn,usvg=error,resvg=error".into(),
    }
}

/// Installs the global subscriber. Returns the log file path when file logging is on.
/// `RUST_LOG` overrides the levels.
pub fn init(verbose: bool, log_file: bool, gui: bool, paths: &Paths) -> anyhow::Result<Option<PathBuf>> {
    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter(verbose, gui)));
    let stderr_layer = fmt::layer().with_writer(std::io::stderr).with_target(verbose).with_filter(filter());
    if !log_file {
        tracing_subscriber::registry().with(stderr_layer).try_init().ok();
        return Ok(None);
    }
    let path = paths.log_file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("could not open the log file {}", path.display()))?;
    let file_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter(verbose, true)));
    let file_layer = fmt::layer().with_ansi(false).with_writer(Mutex::new(file)).with_filter(file_filter);
    tracing_subscriber::registry().with(stderr_layer).with(file_layer).try_init().ok();
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_parse() {
        for (v, g) in [(true, true), (false, true), (false, false)] {
            assert!(EnvFilter::try_new(default_filter(v, g)).is_ok());
        }
    }
}
