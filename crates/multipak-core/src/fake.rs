//! A scriptable [`Host`] for tests: no real commands are run.
//!
//! Register responses with [`FakeHost::on`] (matched on the argument prefix) or
//! [`FakeHost::on_with`] (a closure that can also change files, for example to imitate
//! `flatpak override` writing its keyfile). Every call is recorded.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::error::Result;
use crate::exec::{CmdOutput, CommandSpec, Host, LaunchOutcome};

type Handler = Arc<dyn Fn(&CommandSpec) -> CmdOutput + Send + Sync>;

struct Rule {
    program: String,
    prefix: Vec<String>,
    handler: Handler,
}

/// Simulated process in the fake `/proc`.
#[derive(Debug, Clone, Default)]
pub struct FakeProcess {
    pub environ: Vec<String>,
    pub cmdline: Vec<String>,
    pub children: Vec<u32>,
}

#[derive(Default)]
struct State {
    rules: Vec<Rule>,
    calls: Vec<CommandSpec>,
    launches: Vec<CommandSpec>,
    launch_outcome: Option<LaunchOutcome>,
    processes: HashMap<u32, FakeProcess>,
    executables: HashMap<String, PathBuf>,
}

/// See the [module documentation](self).
#[derive(Clone, Default)]
pub struct FakeHost {
    state: Arc<Mutex<State>>,
}

impl FakeHost {
    /// A fake host where `flatpak` and `update-desktop-database` exist in `/usr/bin`.
    pub fn new() -> Self {
        let host = Self::default();
        host.add_executable("flatpak", "/usr/bin/flatpak");
        host.add_executable("update-desktop-database", "/usr/bin/update-desktop-database");
        host
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn add_executable(&self, name: &str, path: &str) {
        self.lock().executables.insert(name.into(), PathBuf::from(path));
    }

    pub fn remove_executable(&self, name: &str) {
        self.lock().executables.remove(name);
    }

    /// Responds with `output` to `program` called with arguments starting with `prefix`.
    /// Later rules take priority.
    pub fn on(&self, program: &str, prefix: &[&str], output: CmdOutput) {
        self.on_with(program, prefix, move |_| output.clone());
    }

    /// Like [`Self::on`] but computes the response (and side effects) in a closure.
    pub fn on_with(
        &self,
        program: &str,
        prefix: &[&str],
        handler: impl Fn(&CommandSpec) -> CmdOutput + Send + Sync + 'static,
    ) {
        self.lock().rules.push(Rule {
            program: program.into(),
            prefix: prefix.iter().map(|s| s.to_string()).collect(),
            handler: Arc::new(handler),
        });
    }

    /// Sets what [`Host::launch_detached`] returns (default: running with pid 4242).
    pub fn set_launch_outcome(&self, outcome: LaunchOutcome) {
        self.lock().launch_outcome = Some(outcome);
    }

    pub fn add_process(&self, pid: u32, process: FakeProcess) {
        self.lock().processes.insert(pid, process);
    }

    pub fn clear_processes(&self) {
        self.lock().processes.clear();
    }

    /// All commands passed to [`Host::run`], in order.
    pub fn calls(&self) -> Vec<CommandSpec> {
        self.lock().calls.clone()
    }

    /// All commands passed to [`Host::launch_detached`], in order.
    pub fn launches(&self) -> Vec<CommandSpec> {
        self.lock().launches.clone()
    }

    /// Calls whose arguments start with `prefix`.
    pub fn calls_matching(&self, prefix: &[&str]) -> Vec<CommandSpec> {
        self.calls()
            .into_iter()
            .filter(|c| c.args.len() >= prefix.len() && c.args.iter().zip(prefix).all(|(a, p)| a == p))
            .collect()
    }

    fn program_matches(rule_program: &str, spec_program: &str) -> bool {
        spec_program == rule_program
            || Path::new(spec_program).file_name().and_then(|n| n.to_str()) == Some(rule_program)
    }
}

impl Host for FakeHost {
    fn run(&self, spec: &CommandSpec) -> Result<CmdOutput> {
        let handler = {
            let mut state = self.lock();
            state.calls.push(spec.clone());
            state
                .rules
                .iter()
                .rev()
                .find(|r| {
                    Self::program_matches(&r.program, &spec.program)
                        && spec.args.len() >= r.prefix.len()
                        && spec.args.iter().zip(&r.prefix).all(|(a, p)| a == p)
                })
                .map(|r| r.handler.clone())
        };
        Ok(match handler {
            Some(h) => h(spec),
            None => CmdOutput::ok(""),
        })
    }

    fn launch_detached(&self, spec: &CommandSpec, log: &Path, _grace: Duration) -> Result<LaunchOutcome> {
        if let Some(parent) = log.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut state = self.lock();
        state.launches.push(spec.clone());
        Ok(state.launch_outcome.clone().unwrap_or(LaunchOutcome::Running { pid: 4242 }))
    }

    fn which(&self, program: &str) -> Option<PathBuf> {
        self.lock().executables.get(program).cloned()
    }

    fn proc_environ(&self, pid: u32) -> Option<Vec<u8>> {
        self.lock().processes.get(&pid).map(|p| join_nul(&p.environ))
    }

    fn proc_cmdline(&self, pid: u32) -> Option<Vec<u8>> {
        self.lock().processes.get(&pid).map(|p| join_nul(&p.cmdline))
    }

    fn proc_children(&self, pid: u32) -> Vec<u32> {
        self.lock().processes.get(&pid).map(|p| p.children.clone()).unwrap_or_default()
    }
}

fn join_nul(items: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    for item in items {
        out.extend_from_slice(item.as_bytes());
        out.push(0);
    }
    out
}
