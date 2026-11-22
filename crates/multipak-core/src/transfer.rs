//! Import and export of the instance setup as JSON, optionally with data backups.
//!
//! An export is a JSON file. With data, backups are written next to it in a folder named
//! `<file stem>-data/`, and each instance entry points to its archive by relative path.
//! Custom data folders inside the home folder are stored as `~/…` so the setup works on
//! another machine with a different user name.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::backup::{self, CancelToken, Progress, ProgressFn};
use crate::error::{Error, IoContext, Result};
use crate::instance::{Instance, InstanceKey, copy_name, default_wm_class, unique_slug};
use crate::manager::{Manager, RestoreTarget};

pub const EXPORT_FORMAT: &str = "multipak-export";
pub const EXPORT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportedInstance {
    /// The instance, with `data_dir` cleared (see `data_dir` below) and no override record.
    pub instance: Instance,
    /// Custom data folder, with the home folder written as `~`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<String>,
    /// Relative path of the data backup, if included.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportFile {
    pub format: String,
    pub version: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub exported: time::OffsetDateTime,
    pub multipak_version: String,
    pub instances: Vec<ExportedInstance>,
}

#[derive(Debug, Clone, Default)]
pub struct ExportReport {
    pub instances: usize,
    pub backups: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

/// What to do when an imported instance already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnConflict {
    #[default]
    Skip,
    Rename,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ImportOptions {
    pub on_conflict: OnConflict,
    /// Consent to run `flatpak override` for imported custom data folders.
    pub allow_overrides: bool,
    /// Restore included data backups.
    pub restore_data: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ImportReport {
    pub created: Vec<InstanceKey>,
    pub restored: Vec<InstanceKey>,
    pub skipped: Vec<(InstanceKey, String)>,
    pub warnings: Vec<String>,
}

/// Folder holding the backups of an export.
pub fn data_dir_for(export: &Path) -> PathBuf {
    let stem = export.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "multipak-export".into());
    export.with_file_name(format!("{stem}-data"))
}

/// Reads and checks an export file.
pub fn read_export(path: &Path) -> Result<ExportFile> {
    let text = std::fs::read_to_string(path).ctx(|| format!("could not read {}", path.display()))?;
    let file: ExportFile = serde_json::from_str(&text)
        .map_err(|e| Error::Parse { what: path.display().to_string(), reason: e.to_string() })?;
    if file.format != EXPORT_FORMAT {
        return Err(Error::Parse { what: path.display().to_string(), reason: "this is not a Multipak export".into() });
    }
    if file.version > EXPORT_VERSION {
        return Err(Error::Parse {
            what: path.display().to_string(),
            reason: format!("it was made by a newer Multipak (export format {}); please update", file.version),
        });
    }
    Ok(file)
}

impl Manager {
    /// Writes the setup (and optionally data backups) to `path`.
    pub fn export(
        &self,
        path: &Path,
        with_data: bool,
        progress: ProgressFn<'_>,
        cancel: &CancelToken,
    ) -> Result<ExportReport> {
        let config = self.load()?;
        let paths = self.paths();
        let mut report = ExportReport::default();
        let data_folder = data_dir_for(path);
        let mut total = 0u64;
        if with_data {
            for inst in &config.instances {
                let dir = inst.resolved_data_dir(paths);
                if dir.is_dir() {
                    total += backup::data_size(&dir, cancel)?;
                }
            }
        }
        let finished = AtomicU64::new(0);
        let mut entries = Vec::new();
        for inst in &config.instances {
            cancel.check()?;
            let mut copy = inst.clone();
            copy.managed_override = None;
            let data_dir = copy.data_dir.take().map(|d| paths.tildify(&d));
            let mut entry = ExportedInstance { instance: copy, data_dir, backup: None };
            if with_data {
                let dir = inst.resolved_data_dir(paths);
                if dir.is_dir() {
                    let out = backup::backup_file_for_key(&data_folder, inst);
                    let base = finished.load(Ordering::Relaxed);
                    let inner = |p: Progress| {
                        progress(Progress { done: base + p.done, total, current: Some(inst.display_name()) })
                    };
                    let m = backup::create_backup(inst, &dir, &out, &inner, cancel)?;
                    finished.fetch_add(m.total_bytes, Ordering::Relaxed);
                    let rel = format!(
                        "{}/{}",
                        data_folder.file_name().unwrap_or_default().to_string_lossy(),
                        out.file_name().unwrap_or_default().to_string_lossy()
                    );
                    entry.backup = Some(rel);
                    report.backups.push(out);
                } else {
                    report
                        .warnings
                        .push(format!("{} has no data folder yet; exported its settings only.", inst.display_name()));
                }
            }
            entries.push(entry);
        }
        let file = ExportFile {
            format: EXPORT_FORMAT.into(),
            version: EXPORT_VERSION,
            exported: time::OffsetDateTime::now_utc(),
            multipak_version: env!("CARGO_PKG_VERSION").into(),
            instances: entries,
        };
        let json = serde_json::to_vec_pretty(&file)
            .map_err(|e| Error::Parse { what: "export".into(), reason: e.to_string() })?;
        crate::atomic::write_atomic(path, &json, 0o600)?;
        report.instances = file.instances.len();
        Ok(report)
    }

    /// Creates instances from an export file.
    pub fn import(
        &self,
        path: &Path,
        opts: ImportOptions,
        progress: ProgressFn<'_>,
        cancel: &CancelToken,
    ) -> Result<ImportReport> {
        let file = read_export(path)?;
        let base_dir = path.parent().unwrap_or(Path::new("."));
        let mut report = ImportReport::default();
        let installed: Option<Vec<String>> = self.list_apps().ok().map(|a| a.into_iter().map(|a| a.app_id).collect());
        for entry in file.instances {
            cancel.check()?;
            let config = self.load()?;
            let mut draft = entry.instance.to_draft();
            draft.data_dir = entry.data_dir.as_deref().map(|d| self.paths().expand_tilde(d));
            let original = InstanceKey::new(&draft.app_id, &draft.slug);
            if config.get(&original).is_some() {
                match opts.on_conflict {
                    OnConflict::Skip => {
                        report.skipped.push((original, "an instance with this short name already exists".into()));
                        continue;
                    }
                    OnConflict::Rename => {
                        draft.slug =
                            unique_slug(&draft.slug, |s| config.get(&InstanceKey::new(&draft.app_id, s)).is_some());
                        draft.wm_class = default_wm_class(&draft.app_id, &draft.slug);
                        draft.name = copy_name(&draft.name);
                    }
                }
            }
            if config.instances.iter().any(|i| i.wm_class == draft.wm_class) {
                draft.wm_class = default_wm_class(&draft.app_id, &draft.slug);
            }
            if let Some(list) = &installed
                && !list.contains(&draft.app_id)
            {
                report.warnings.push(format!(
                    "{} is not installed on this computer; install it before launching {}.",
                    draft.app_id, draft.name
                ));
            }
            let created = match self.create(&draft, opts.allow_overrides) {
                Ok(o) => {
                    report.warnings.extend(o.warnings);
                    o.instance
                }
                Err(e) => {
                    report.skipped.push((original, e.to_string()));
                    continue;
                }
            };
            let key = created.key();
            report.created.push(key.clone());
            if let (true, Some(rel)) = (opts.restore_data, &entry.backup) {
                let archive = base_dir.join(rel);
                if !archive.is_file() {
                    report.warnings.push(format!(
                        "The data backup {} is missing; {} starts empty.",
                        archive.display(),
                        created.name
                    ));
                    continue;
                }
                let inner = |p: Progress| progress(Progress { current: Some(created.display_name()), ..p });
                self.restore(&archive, RestoreTarget::Existing(key.clone()), &inner, cancel)?;
                report.restored.push(key);
            }
        }
        Ok(report)
    }
}
