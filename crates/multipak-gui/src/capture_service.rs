//! `multipak capture-service`: answers the KWin capture script over D-Bus.
//!
//! D-Bus starts this process on demand (see `multipak_core::capture`). For each window, the
//! script sends the window's process ID; the answer says whether the window belongs to a
//! Multipak instance that hides from capture. The process exits after a few idle minutes.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use multipak_core::Manager;
use multipak_core::capture::{self, DECISION_NOT_OURS};

const IDLE_EXIT: Duration = Duration::from_secs(300);

struct Capture {
    manager: Arc<Manager>,
    last_call: Arc<Mutex<Instant>>,
}

#[zbus::interface(name = "io.github.minasaad47.Multipak.Capture")]
impl Capture {
    /// `1` hide, `0` show (a Multipak instance that is not hidden), `-1` not a Multipak instance.
    fn should_hide(&self, pid: &str) -> i32 {
        if let Ok(mut t) = self.last_call.lock() {
            *t = Instant::now();
        }
        let Ok(pid) = pid.trim().parse::<u32>() else {
            return DECISION_NOT_OURS;
        };
        match self.manager.load() {
            Ok(config) => capture::decide(self.manager.host(), &config, pid),
            Err(e) => {
                tracing::warn!("could not read the instance setup: {e}");
                DECISION_NOT_OURS
            }
        }
    }
}

pub fn run(manager: Arc<Manager>) -> anyhow::Result<()> {
    let last_call = Arc::new(Mutex::new(Instant::now()));
    let service = Capture { manager, last_call: last_call.clone() };
    let _connection = zbus::blocking::connection::Builder::session()
        .context("could not connect to the session bus")?
        .name(capture::SERVICE_NAME)
        .context("could not claim the D-Bus name")?
        .serve_at(capture::SERVICE_PATH, service)
        .context("could not export the D-Bus object")?
        .build()
        .context("could not start the D-Bus service (is another copy running?)")?;
    tracing::info!("capture service running");
    loop {
        std::thread::sleep(Duration::from_secs(5));
        let idle = last_call.lock().map(|t| t.elapsed()).unwrap_or(IDLE_EXIT);
        if idle >= IDLE_EXIT {
            tracing::info!("capture service idle; exiting");
            return Ok(());
        }
    }
}
