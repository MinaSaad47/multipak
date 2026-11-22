//! Command execution and process inspection, abstracted behind [`Host`] so the rest
//! of the library can be tested without a real Flatpak installation.
//!
//! Commands are always run with [`std::process::Command`] and a separate argument
//! list; nothing is ever passed through a shell.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::error::{Error, IoContext, Result};

/// A command to run: program, separate arguments, extra environment and an optional timeout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub timeout: Option<Duration>,
}

impl CommandSpec {
    pub fn new(program: impl Into<String>) -> Self {
        Self { program: program.into(), args: Vec::new(), env: Vec::new(), timeout: None }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// The program followed by its arguments.
    pub fn argv(&self) -> Vec<String> {
        std::iter::once(self.program.clone()).chain(self.args.iter().cloned()).collect()
    }

    /// A shell-quoted rendering for display only (previews, logs, error messages).
    /// It is never executed.
    pub fn display(&self) -> String {
        shell_display(&self.argv())
    }
}

/// Quotes arguments the way a POSIX shell would need them, for display purposes.
pub fn shell_display(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            let safe = !a.is_empty() && a.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%^".contains(c));
            if safe { a.clone() } else { format!("'{}'", a.replace('\'', r"'\''")) }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Captured result of a finished command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CmdOutput {
    /// Exit code, or `None` if the process was killed by a signal.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CmdOutput {
    pub fn ok(stdout: impl Into<String>) -> Self {
        Self { code: Some(0), stdout: stdout.into(), stderr: String::new() }
    }

    pub fn failed(code: i32, stderr: impl Into<String>) -> Self {
        Self { code: Some(code), stdout: String::new(), stderr: stderr.into() }
    }

    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// Converts a non-zero exit into [`Error::CommandFailed`].
    pub fn check(self, spec: &CommandSpec) -> Result<Self> {
        if self.success() {
            Ok(self)
        } else {
            Err(Error::CommandFailed { command: spec.display(), code: self.code, stderr: self.stderr })
        }
    }
}

/// What happened right after a detached launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchOutcome {
    /// Still running after the grace period; it keeps running in the background.
    Running { pid: u32 },
    /// Exited with code 0 within the grace period (for example, it handed off to an
    /// already-running copy).
    ExitedOk,
    /// Exited with an error within the grace period.
    Failed { code: Option<i32>, log_tail: String },
}

/// Everything Multipak needs from the operating system.
pub trait Host: Send + Sync {
    /// Runs a command to completion and captures its output.
    fn run(&self, spec: &CommandSpec) -> Result<CmdOutput>;

    /// Starts a command in the background with stdout/stderr going to `log`, then waits
    /// up to `grace` to see whether it fails immediately.
    fn launch_detached(&self, spec: &CommandSpec, log: &Path, grace: Duration) -> Result<LaunchOutcome>;

    /// Finds an executable in `PATH`.
    fn which(&self, program: &str) -> Option<PathBuf>;

    /// Raw `/proc/<pid>/environ` (NUL separated), if readable.
    fn proc_environ(&self, pid: u32) -> Option<Vec<u8>>;

    /// Raw `/proc/<pid>/cmdline` (NUL separated), if readable.
    fn proc_cmdline(&self, pid: u32) -> Option<Vec<u8>>;

    /// Direct child processes of `pid`.
    fn proc_children(&self, pid: u32) -> Vec<u32>;

    /// Parent process of `pid`, if known.
    fn proc_parent(&self, pid: u32) -> Option<u32>;
}

/// The real implementation of [`Host`].
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemHost;

impl SystemHost {
    fn command(spec: &CommandSpec) -> Command {
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args);
        for (key, value) in sanitized_env_changes(|k| std::env::var_os(k)) {
            match value {
                Some(v) => cmd.env(key, v),
                None => cmd.env_remove(key),
            };
        }
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::null());
        cmd
    }
}

/// When Multipak runs from an AppImage, the AppImage runtime adds variables (and PATH
/// entries inside the mounted image) that must not leak into launched apps. Returns the
/// environment changes needed to undo that: `None` means "remove the variable".
pub fn sanitized_env_changes(lookup: impl Fn(&str) -> Option<OsString>) -> Vec<(String, Option<OsString>)> {
    let Some(appdir) = lookup("APPDIR").filter(|d| !d.is_empty()) else {
        return Vec::new();
    };
    let appdir = PathBuf::from(appdir);
    let mut changes: Vec<(String, Option<OsString>)> =
        ["APPDIR", "APPIMAGE", "ARGV0", "OWD"].iter().map(|k| (k.to_string(), None)).collect();
    for key in
        ["PATH", "LD_LIBRARY_PATH", "XDG_DATA_DIRS", "GSETTINGS_SCHEMA_DIR", "QT_PLUGIN_PATH", "PYTHONPATH", "PERLLIB"]
    {
        let Some(value) = lookup(key) else { continue };
        let kept: Vec<PathBuf> = std::env::split_paths(&value).filter(|p| !p.starts_with(&appdir)).collect();
        if kept.is_empty() {
            changes.push((key.to_string(), None));
        } else if let Ok(joined) = std::env::join_paths(kept) {
            changes.push((key.to_string(), Some(joined)));
        }
    }
    changes
}

fn read_to_string_lossy(mut r: impl Read) -> String {
    let mut buf = Vec::new();
    let _ = r.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

fn wait_with_timeout(child: &mut Child, timeout: Duration) -> std::io::Result<Option<std::process::ExitStatus>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(25));
    }
}

/// Last `max_lines` lines (at most 8 KiB) of a log file.
pub fn log_tail(path: &Path, max_lines: usize) -> String {
    let Ok(mut file) = File::open(path) else {
        return String::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(8192);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let text = read_to_string_lossy(file);
    let lines: Vec<&str> = text.lines().collect();
    let from = lines.len().saturating_sub(max_lines);
    lines[from..].join("\n")
}

impl Host for SystemHost {
    fn run(&self, spec: &CommandSpec) -> Result<CmdOutput> {
        tracing::debug!(command = %spec.display(), "running");
        let mut cmd = Self::command(spec);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|source| Error::Spawn { program: spec.program.clone(), source })?;
        let stdout = child.stdout.take().map(|s| thread::spawn(move || read_to_string_lossy(s)));
        let stderr = child.stderr.take().map(|s| thread::spawn(move || read_to_string_lossy(s)));
        let status = match spec.timeout {
            Some(t) => match wait_with_timeout(&mut child, t).ctx(|| format!("could not wait for {}", spec.program))? {
                Some(status) => status,
                None => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(Error::CommandTimeout { command: spec.display(), secs: t.as_secs() });
                }
            },
            None => child.wait().ctx(|| format!("could not wait for {}", spec.program))?,
        };
        let stdout = stdout.and_then(|h| h.join().ok()).unwrap_or_default();
        let stderr = stderr.and_then(|h| h.join().ok()).unwrap_or_default();
        Ok(CmdOutput { code: status.code(), stdout, stderr })
    }

    fn launch_detached(&self, spec: &CommandSpec, log: &Path, grace: Duration) -> Result<LaunchOutcome> {
        tracing::info!(command = %spec.display(), "launching");
        if let Some(parent) = log.parent() {
            fs::create_dir_all(parent).ctx(|| format!("could not create {}", parent.display()))?;
        }
        let log_file = File::create(log).ctx(|| format!("could not create {}", log.display()))?;
        let log_err = log_file.try_clone().ctx(|| format!("could not open {}", log.display()))?;
        let mut cmd = Self::command(spec);
        cmd.stdout(log_file).stderr(log_err);
        // Own process group: closing the terminal or pressing Ctrl+C in it does not
        // take the launched app down with it.
        cmd.process_group(0);
        let mut child = cmd.spawn().map_err(|source| Error::Spawn { program: spec.program.clone(), source })?;
        let pid = child.id();
        match wait_with_timeout(&mut child, grace).ctx(|| format!("could not wait for {}", spec.program))? {
            Some(status) if status.success() => Ok(LaunchOutcome::ExitedOk),
            Some(status) => Ok(LaunchOutcome::Failed { code: status.code(), log_tail: log_tail(log, 25) }),
            None => {
                // Reap the child when it eventually exits so it never becomes a zombie
                // while the GUI keeps running.
                thread::Builder::new()
                    .name(format!("reaper-{pid}"))
                    .spawn(move || {
                        let _ = child.wait();
                    })
                    .ctx(|| "could not start a helper thread".to_string())?;
                Ok(LaunchOutcome::Running { pid })
            }
        }
    }

    fn which(&self, program: &str) -> Option<PathBuf> {
        which_in(program, std::env::var_os("PATH"))
    }

    fn proc_environ(&self, pid: u32) -> Option<Vec<u8>> {
        fs::read(format!("/proc/{pid}/environ")).ok()
    }

    fn proc_cmdline(&self, pid: u32) -> Option<Vec<u8>> {
        fs::read(format!("/proc/{pid}/cmdline")).ok()
    }

    fn proc_parent(&self, pid: u32) -> Option<u32> {
        parse_stat_ppid(&fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
    }

    fn proc_children(&self, pid: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let Ok(tasks) = fs::read_dir(format!("/proc/{pid}/task")) else {
            return out;
        };
        for task in tasks.flatten() {
            if let Ok(text) = fs::read_to_string(task.path().join("children")) {
                out.extend(text.split_whitespace().filter_map(|s| s.parse::<u32>().ok()));
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }
}

/// Searches `path_var` for an executable named `program`.
pub fn which_in(program: &str, path_var: Option<OsString>) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    if program.contains('/') {
        let p = PathBuf::from(program);
        return p.is_file().then_some(p);
    }
    let path_var = path_var?;
    std::env::split_paths(&path_var).filter(|dir| dir.is_absolute()).map(|dir| dir.join(program)).find(|candidate| {
        fs::metadata(candidate).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
    })
}

/// Parent PID from `/proc/<pid>/stat`. The process name (field 2) may contain spaces and
/// parentheses, so parsing starts after its last `)`.
pub fn parse_stat_ppid(stat: &str) -> Option<u32> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// Splits NUL separated `/proc` data into strings.
pub fn split_nul(data: &[u8]) -> impl Iterator<Item = String> + '_ {
    data.split(|b| *b == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn display_quotes_only_when_needed() {
        let argv = vec!["/usr/bin/flatpak".to_string(), "run".into(), "--env=A=b c".into(), "it's".into(), "".into()];
        assert_eq!(shell_display(&argv), r#"/usr/bin/flatpak run '--env=A=b c' 'it'\''s' ''"#);
    }

    #[test]
    fn system_host_runs_without_a_shell() {
        let out = SystemHost.run(&CommandSpec::new("printf").args(["%s|", "a b", "$HOME"])).unwrap();
        assert!(out.success());
        assert_eq!(out.stdout, "a b|$HOME|");
    }

    #[test]
    fn system_host_reports_failures_and_timeouts() {
        let out = SystemHost.run(&CommandSpec::new("sh").args(["-c", "echo oops >&2; exit 3"])).unwrap();
        assert_eq!(out.code, Some(3));
        assert_eq!(out.stderr.trim(), "oops");
        let err = SystemHost.run(&CommandSpec::new("sleep").arg("5").timeout(Duration::from_millis(200))).unwrap_err();
        assert!(matches!(err, Error::CommandTimeout { .. }));
        let err = SystemHost.run(&CommandSpec::new("/nonexistent/program")).unwrap_err();
        assert!(matches!(err, Error::Spawn { .. }));
    }

    #[test]
    fn detached_launch_detects_immediate_failure() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("l.log");
        let failed = SystemHost
            .launch_detached(&CommandSpec::new("sh").args(["-c", "echo broken; exit 7"]), &log, Duration::from_secs(5))
            .unwrap();
        assert_eq!(failed, LaunchOutcome::Failed { code: Some(7), log_tail: "broken".into() });
        let running = SystemHost
            .launch_detached(&CommandSpec::new("sleep").arg("0.6"), &log, Duration::from_millis(100))
            .unwrap();
        assert!(matches!(running, LaunchOutcome::Running { .. }));
    }

    #[test]
    fn appimage_environment_is_cleaned() {
        let env: HashMap<&str, &str> = HashMap::from([
            ("APPDIR", "/tmp/.mount_x"),
            ("APPIMAGE", "/home/u/Multipak.AppImage"),
            ("PATH", "/tmp/.mount_x/usr/bin:/usr/bin:/bin"),
            ("LD_LIBRARY_PATH", "/tmp/.mount_x/usr/lib"),
        ]);
        let changes = sanitized_env_changes(|k| env.get(k).map(OsString::from));
        let map: HashMap<_, _> = changes.into_iter().collect();
        assert_eq!(map["APPDIR"], None);
        assert_eq!(map["APPIMAGE"], None);
        assert_eq!(map["PATH"], Some(OsString::from("/usr/bin:/bin")));
        assert_eq!(map["LD_LIBRARY_PATH"], None);
        assert!(sanitized_env_changes(|_| None).is_empty());
    }

    #[test]
    fn parses_parent_pid() {
        assert_eq!(parse_stat_ppid("1234 (my (odd) name) S 42 1234 1234 0 -1"), Some(42));
        assert_eq!(parse_stat_ppid("garbage"), None);
        assert_eq!(SystemHost.proc_parent(std::process::id()), Some(std::os::unix::process::parent_id()));
    }

    #[test]
    fn which_finds_executables() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("tool");
        fs::write(&exe, "#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        let plain = dir.path().join("plain");
        fs::write(&plain, "").unwrap();
        let path = std::env::join_paths([dir.path()]).unwrap();
        assert_eq!(which_in("tool", Some(path.clone())), Some(exe));
        assert_eq!(which_in("plain", Some(path.clone())), None);
        assert_eq!(which_in("missing", Some(path)), None);
    }
}
