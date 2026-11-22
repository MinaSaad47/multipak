//! Background work for the GUI. The UI thread never runs Flatpak commands, file scans,
//! backups or file dialogs itself: it submits closures here and receives results over a
//! channel. Every finished job (and every progress update) calls `request_repaint`.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use multipak_core::backup::{BackupManifest, CancelToken, Progress};
use multipak_core::doctor::Issue;
use multipak_core::manager::{AppDetails, CacheReport, DeleteReport, RunningInstance, SaveOutcome};
use multipak_core::transfer::{ExportFile, ExportReport, ImportReport};
use multipak_core::{Config, Error, FlatpakApp, Instance, InstanceKey, LaunchOutcome};

pub type JobId = u64;

/// What a file dialog was opened for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickPurpose {
    FormDataDir,
    BackupTo(InstanceKey),
    RestoreInto(InstanceKey),
    RestoreAsNew,
    ExportTo { with_data: bool },
    ImportFrom,
}

/// Results of background jobs.
pub enum Output {
    Apps(Result<Vec<FlatpakApp>, Error>),
    Config(Result<Config, Error>),
    FlatpakBinary(Option<PathBuf>),
    Running(Result<Vec<RunningInstance>, Error>),
    Details(AppDetails),
    Icons(Vec<(String, Option<PathBuf>)>),
    DiskUsage(InstanceKey, Result<u64, Error>),
    Saved { editing: bool, result: Result<SaveOutcome, Error> },
    Duplicated(Result<SaveOutcome, Error>),
    Deleted(Result<DeleteReport, Error>),
    Launched(InstanceKey, Result<LaunchOutcome, Error>),
    Stopped(InstanceKey, Result<usize, Error>),
    CacheCleared(InstanceKey, Result<CacheReport, Error>),
    BackedUp(PathBuf, Result<BackupManifest, Error>),
    Restored(Result<Instance, Error>),
    Doctor(Result<Vec<Issue>, Error>),
    FixApplied(Result<(), Error>),
    ExportRead(PathBuf, Result<ExportFile, Error>),
    Exported(PathBuf, Result<ExportReport, Error>),
    Imported(Result<ImportReport, Error>),
    Picked(PickPurpose, Option<PathBuf>),
    Opened(Result<(), String>),
}

pub enum Msg {
    Progress(JobId, Progress),
    Done(JobId, Box<Output>),
}

/// Handed to every job.
pub struct JobCtx {
    id: JobId,
    tx: Sender<Msg>,
    ctx: egui::Context,
    pub cancel: CancelToken,
}

impl JobCtx {
    pub fn progress(&self, p: Progress) {
        let _ = self.tx.send(Msg::Progress(self.id, p));
        self.ctx.request_repaint();
    }
}

type JobFn = Box<dyn FnOnce(&JobCtx) -> Output + Send>;

struct Job {
    ctx: JobCtx,
    f: JobFn,
}

fn run(job: Job) {
    let Job { ctx, f } = job;
    let id = ctx.id;
    // A panicking job must not take the UI down; report it as an error instead.
    let output = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&ctx)));
    match output {
        Ok(out) => {
            let _ = ctx.tx.send(Msg::Done(id, Box::new(out)));
        }
        Err(_) => {
            tracing::error!("background job {id} panicked");
            let _ = ctx.tx.send(Msg::Done(
                id,
                Box::new(Output::Opened(Err(
                    "An internal error occurred in a background task. See the log for details.".into(),
                ))),
            ));
        }
    }
    ctx.ctx.request_repaint();
}

/// A small pool for quick jobs plus dedicated threads for long ones.
pub struct Worker {
    ctx: egui::Context,
    jobs: Sender<Job>,
    results_tx: Sender<Msg>,
    results: Receiver<Msg>,
    next_id: JobId,
}

impl Worker {
    pub fn new(ctx: egui::Context, threads: usize) -> Self {
        let (jobs, job_rx) = mpsc::channel::<Job>();
        let job_rx = Arc::new(Mutex::new(job_rx));
        for n in 0..threads.max(1) {
            let rx = job_rx.clone();
            let spawned = thread::Builder::new().name(format!("multipak-worker-{n}")).spawn(move || {
                loop {
                    let job = {
                        let Ok(guard) = rx.lock() else { return };
                        guard.recv()
                    };
                    match job {
                        Ok(job) => run(job),
                        Err(_) => return,
                    }
                }
            });
            if let Err(e) = spawned {
                tracing::error!("could not start worker thread: {e}");
            }
        }
        let (results_tx, results) = mpsc::channel();
        Self { ctx, jobs, results_tx, results, next_id: 1 }
    }

    fn make(&mut self, f: JobFn) -> (JobId, CancelToken, Job) {
        let id = self.next_id;
        self.next_id += 1;
        let cancel = CancelToken::new();
        let job =
            Job { ctx: JobCtx { id, tx: self.results_tx.clone(), ctx: self.ctx.clone(), cancel: cancel.clone() }, f };
        (id, cancel, job)
    }

    /// Runs a quick job on the pool.
    pub fn spawn(&mut self, f: impl FnOnce(&JobCtx) -> Output + Send + 'static) -> JobId {
        let (id, _, job) = self.make(Box::new(f));
        if let Err(mpsc::SendError(job)) = self.jobs.send(job) {
            // The pool is gone (should not happen); run on a fresh thread instead.
            thread::spawn(move || run(job));
        }
        id
    }

    /// Runs a long or blocking job (backups, file dialogs) on its own thread.
    pub fn spawn_long(
        &mut self,
        name: &str,
        f: impl FnOnce(&JobCtx) -> Output + Send + 'static,
    ) -> (JobId, CancelToken) {
        let (id, cancel, job) = self.make(Box::new(f));
        let spawned = thread::Builder::new().name(format!("multipak-{name}")).spawn(move || run(job));
        if let Err(e) = spawned {
            let _ = self
                .results_tx
                .send(Msg::Done(id, Box::new(Output::Opened(Err(format!("could not start a background task: {e}"))))));
        }
        (id, cancel)
    }

    /// Messages that arrived since the last call.
    pub fn drain(&self) -> Vec<Msg> {
        self.results.try_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait_for(worker: &Worker, n: usize) -> Vec<Msg> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut all = Vec::new();
        while all.len() < n && Instant::now() < deadline {
            all.extend(worker.drain());
            thread::sleep(Duration::from_millis(5));
        }
        all
    }

    #[test]
    fn jobs_report_progress_and_results() {
        let mut w = Worker::new(egui::Context::default(), 2);
        let id = w.spawn(|ctx| {
            ctx.progress(Progress { done: 1, total: 2, current: None });
            Output::FlatpakBinary(Some(PathBuf::from("/usr/bin/flatpak")))
        });
        let msgs = wait_for(&w, 2);
        assert!(matches!(msgs[0], Msg::Progress(i, _) if i == id));
        assert!(matches!(&msgs[1], Msg::Done(i, o) if *i == id && matches!(**o, Output::FlatpakBinary(Some(_)))));
    }

    #[test]
    fn long_jobs_can_be_cancelled_and_panics_are_contained() {
        let mut w = Worker::new(egui::Context::default(), 1);
        let (_, cancel) = w.spawn_long("test", |ctx| {
            while !ctx.cancel.is_cancelled() {
                thread::sleep(Duration::from_millis(2));
            }
            Output::Opened(Err("cancelled".into()))
        });
        cancel.cancel();
        let msgs = wait_for(&w, 1);
        assert!(matches!(&msgs[0], Msg::Done(_, o) if matches!(&**o, Output::Opened(Err(e)) if e == "cancelled")));

        w.spawn(|_| panic!("boom"));
        let msgs = wait_for(&w, 1);
        assert!(matches!(&msgs[0], Msg::Done(_, o) if matches!(**o, Output::Opened(Err(_)))));
    }
}
