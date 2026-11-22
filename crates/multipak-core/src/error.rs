//! Typed errors for the core library.

use std::path::PathBuf;

/// Result alias used throughout the crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Every error the core library can return. Messages are written for end users.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },

    #[error("could not start `{program}`: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },

    #[error("`{command}` failed{}: {}", exit_suffix(*.code), stderr_or_default(.stderr))]
    CommandFailed { command: String, code: Option<i32>, stderr: String },

    #[error("`{command}` did not finish within {secs} seconds and was stopped")]
    CommandTimeout { command: String, secs: u64 },

    #[error("the `flatpak` command was not found; install Flatpak and make sure it is in PATH")]
    FlatpakMissing,

    #[error("bubblewrap (`bwrap`) was not found; it comes with Flatpak, so reinstall Flatpak or install bubblewrap")]
    BwrapMissing,

    #[error("invalid {field}: {reason}")]
    Invalid { field: &'static str, reason: String },

    #[error("could not read {what}: {reason}")]
    Parse { what: String, reason: String },

    #[error(
        "{} was written by a newer version of Multipak (format version {found}; this version supports up to {supported}). Please update Multipak.",
        .path.display()
    )]
    ConfigTooNew { path: PathBuf, found: u32, supported: u32 },

    #[error("no instance matches `{0}`")]
    InstanceNotFound(String),

    #[error("`{query}` matches more than one instance; use one of: {candidates}")]
    AmbiguousInstance { query: String, candidates: String },

    #[error("{app_id} already has an instance with the short name `{slug}`")]
    DuplicateSlug { app_id: String, slug: String },

    #[error("{0} is not installed")]
    AppNotInstalled(String),

    #[error(
        "{app_id} cannot reach {} from its sandbox. Multipak needs to run `flatpak override --user --filesystem={} {app_id}`; allow this to continue.",
        .dir.display(), .dir.display()
    )]
    OverrideRequired { app_id: String, dir: PathBuf },

    #[error("{instance} exited right after starting{}. Output:\n{log}", exit_suffix(*.code))]
    LaunchFailed { instance: String, code: Option<i32>, log: String },

    #[error("the operation was cancelled")]
    Cancelled,

    #[error("refusing to delete {}: {reason}", .path.display())]
    UnsafeDelete { path: PathBuf, reason: String },

    #[error("{0} is running; stop it first")]
    InstanceRunning(String),

    #[error("{} is not a valid Multipak backup: {reason}", .path.display())]
    InvalidBackup { path: PathBuf, reason: String },

    #[error("could not render icon: {0}")]
    Icon(String),
}

fn exit_suffix(code: Option<i32>) -> String {
    match code {
        Some(c) => format!(" with exit code {c}"),
        None => " (killed by a signal)".to_string(),
    }
}

fn stderr_or_default(stderr: &str) -> String {
    let trimmed = stderr.trim();
    if trimmed.is_empty() { "no error output".to_string() } else { trimmed.to_string() }
}

impl Error {
    /// Shorthand for an [`Error::Invalid`].
    pub fn invalid(field: &'static str, reason: impl Into<String>) -> Self {
        Self::Invalid { field, reason: reason.into() }
    }
}

/// Adds a human readable context to `std::io::Result`.
pub trait IoContext<T> {
    fn ctx(self, context: impl FnOnce() -> String) -> Result<T>;
}

impl<T> IoContext<T> for std::io::Result<T> {
    fn ctx(self, context: impl FnOnce() -> String) -> Result<T> {
        self.map_err(|source| Error::Io { context: context(), source })
    }
}
