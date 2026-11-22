//! The instance setup file, `~/.config/multipak/instances.toml`.
//!
//! The file carries a `version` field. Older versions are upgraded step by step through
//! [`MIGRATIONS`] (a copy of the old file is kept as `instances.toml.v<N>.bak`), and files
//! from a newer Multipak are refused instead of being silently rewritten.
//!
//! Every change goes through [`ConfigStore::update`], which takes an exclusive file lock,
//! re-reads the file, applies the change and writes it atomically, so the GUI and the CLI
//! can be used at the same time.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::atomic::write_atomic;
use crate::error::{Error, IoContext, Result};
use crate::instance::{Instance, InstanceKey};

/// The format version this build writes.
pub const CURRENT_VERSION: u32 = 1;

/// One upgrade step; `MIGRATIONS[n]` turns version `n` into version `n + 1`.
pub type Migration = fn(&mut toml::Table) -> Result<()>;

/// Version 0 is a file without a `version` field (for example written by hand from the
/// README). Its layout is identical to version 1, so the step only stamps the version.
fn migrate_v0_to_v1(_table: &mut toml::Table) -> Result<()> {
    Ok(())
}

/// Upgrade steps, indexed by the version they start from.
pub const MIGRATIONS: &[Migration] = &[migrate_v0_to_v1];

/// The whole setup file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub version: u32,
    #[serde(default, rename = "instance")]
    pub instances: Vec<Instance>,
}

impl Default for Config {
    fn default() -> Self {
        Self { version: CURRENT_VERSION, instances: Vec::new() }
    }
}

impl Config {
    pub fn get(&self, key: &InstanceKey) -> Option<&Instance> {
        self.instances.iter().find(|i| i.app_id == key.app_id && i.slug == key.slug)
    }

    pub fn get_mut(&mut self, key: &InstanceKey) -> Option<&mut Instance> {
        self.instances.iter_mut().find(|i| i.app_id == key.app_id && i.slug == key.slug)
    }

    pub fn for_app<'a>(&'a self, app_id: &'a str) -> impl Iterator<Item = &'a Instance> + 'a {
        self.instances.iter().filter(move |i| i.app_id == app_id)
    }

    /// Finds an instance by `<app-id>/<slug>`, by slug alone, or by name (case-insensitive)
    /// when that is unambiguous.
    pub fn find(&self, query: &str) -> Result<&Instance> {
        if let Some(key) = InstanceKey::parse(query) {
            return self.get(&key).ok_or_else(|| Error::InstanceNotFound(query.to_string()));
        }
        let by_slug: Vec<&Instance> = self.instances.iter().filter(|i| i.slug == query).collect();
        let matches = if by_slug.is_empty() {
            self.instances
                .iter()
                .filter(|i| i.name.eq_ignore_ascii_case(query) || i.display_name().eq_ignore_ascii_case(query))
                .collect()
        } else {
            by_slug
        };
        match matches.as_slice() {
            [] => Err(Error::InstanceNotFound(query.to_string())),
            [one] => Ok(one),
            many => Err(Error::AmbiguousInstance {
                query: query.to_string(),
                candidates: many.iter().map(|i| i.key().to_string()).collect::<Vec<_>>().join(", "),
            }),
        }
    }

    fn sort(&mut self) {
        self.instances.sort_by(|a, b| {
            a.app_id.cmp(&b.app_id).then_with(|| a.created.cmp(&b.created)).then_with(|| a.slug.cmp(&b.slug))
        });
    }
}

/// Applies `migrations` to bring `table` from its version to `target`.
pub fn migrate_table(table: &mut toml::Table, path: &Path, target: u32, migrations: &[Migration]) -> Result<u32> {
    let found = match table.get("version") {
        None => 0,
        Some(toml::Value::Integer(v)) if *v >= 0 && *v <= i64::from(u32::MAX) => *v as u32,
        Some(other) => {
            return Err(Error::Parse {
                what: path.display().to_string(),
                reason: format!("`version` must be a non-negative integer, found {other}"),
            });
        }
    };
    if found > target {
        return Err(Error::ConfigTooNew { path: path.to_path_buf(), found, supported: target });
    }
    for v in found..target {
        let step = migrations.get(v as usize).ok_or_else(|| Error::Parse {
            what: path.display().to_string(),
            reason: format!("no upgrade path from format version {v}"),
        })?;
        step(table)?;
        table.insert("version".into(), toml::Value::Integer(i64::from(v + 1)));
        tracing::info!(from = v, to = v + 1, "migrated {}", path.display());
    }
    Ok(found)
}

/// Parses setup-file text, upgrading old versions. Returns the config and the version
/// the text was written in.
pub fn parse(text: &str, path: &Path) -> Result<(Config, u32)> {
    let mut table: toml::Table =
        toml::from_str(text).map_err(|e| Error::Parse { what: path.display().to_string(), reason: e.to_string() })?;
    let found = migrate_table(&mut table, path, CURRENT_VERSION, MIGRATIONS)?;
    let mut config: Config = toml::Value::Table(table)
        .try_into()
        .map_err(|e: toml::de::Error| Error::Parse { what: path.display().to_string(), reason: e.to_string() })?;
    config.sort();
    Ok((config, found))
}

/// Serializes a config.
pub fn to_text(config: &Config) -> Result<String> {
    let body = toml::to_string_pretty(config)
        .map_err(|e| Error::Parse { what: "instance setup".into(), reason: e.to_string() })?;
    Ok(format!(
        "# Multipak instance setup. Edit with care: Multipak rewrites this file.\n# Format documentation: https://github.com/MinaSaad47/multipak#configuration-file\n\n{body}"
    ))
}

/// Reads and writes the setup file with locking.
#[derive(Debug, Clone)]
pub struct ConfigStore {
    path: PathBuf,
}

impl ConfigStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn lock_path(&self) -> PathBuf {
        self.path.with_extension("lock")
    }

    fn lock(&self, exclusive: bool) -> Result<fs::File> {
        let lock = self.lock_path();
        if let Some(parent) = lock.parent() {
            fs::create_dir_all(parent).ctx(|| format!("could not create {}", parent.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock)
            .ctx(|| format!("could not open {}", lock.display()))?;
        if exclusive { file.lock() } else { file.lock_shared() }
            .ctx(|| format!("could not lock {}", lock.display()))?;
        Ok(file)
    }

    fn read_unlocked(&self) -> Result<(Config, bool)> {
        match fs::read_to_string(&self.path) {
            Ok(text) => {
                let (config, found) = parse(&text, &self.path)?;
                if found < CURRENT_VERSION {
                    let backup = PathBuf::from(format!("{}.v{found}.bak", self.path.display()));
                    write_atomic(&backup, text.as_bytes(), 0o600)?;
                }
                Ok((config, found < CURRENT_VERSION))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((Config::default(), false)),
            Err(e) => Err(e).ctx(|| format!("could not read {}", self.path.display())),
        }
    }

    /// Reads the current setup (an empty one if the file does not exist yet).
    pub fn load(&self) -> Result<Config> {
        let (config, migrated) = {
            let _lock = self.lock(false)?;
            self.read_unlocked()?
        };
        if migrated {
            // Persist the upgrade so the backup is only written once.
            self.update(|_| Ok(()))?;
        }
        Ok(config)
    }

    /// Applies `change` to the latest setup and saves it atomically while holding an
    /// exclusive lock. Nothing is written if `change` fails.
    pub fn update<R>(&self, change: impl FnOnce(&mut Config) -> Result<R>) -> Result<R> {
        let _lock = self.lock(true)?;
        let (mut config, _) = self.read_unlocked()?;
        let result = change(&mut config)?;
        config.version = CURRENT_VERSION;
        config.sort();
        write_atomic(&self.path, to_text(&config)?.as_bytes(), 0o600)?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apptype::Strategy;
    use crate::instance::InstanceDraft;
    use crate::paths::Paths;

    fn sample_instance(slug: &str) -> Instance {
        let paths = Paths::for_home("/home/u");
        let mut d = InstanceDraft::new("com.slack.Slack", "Slack", slug, Strategy::Electron, &[]);
        d.extra_args = vec!["--enable-features=X".into()];
        d.env.insert("LANG".into(), "de_DE.UTF-8".into());
        d.validate(crate::instance::DraftContext { paths: &paths, existing: &[], editing: None }).unwrap().instance
    }

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::new(dir.path().join("instances.toml"));
        assert_eq!(store.load().unwrap(), Config::default());
        store
            .update(|c| {
                c.instances.push(sample_instance("work"));
                c.instances.push(sample_instance("home"));
                Ok(())
            })
            .unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.instances.len(), 2);
        assert_eq!(loaded.version, CURRENT_VERSION);
        assert_eq!(loaded.find("work").unwrap().env["LANG"], "de_DE.UTF-8");
        assert_eq!(loaded.find("com.slack.Slack/home").unwrap().slug, "home");
        assert!(matches!(loaded.find("nope"), Err(Error::InstanceNotFound(_))));
    }

    #[test]
    fn failed_update_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::new(dir.path().join("instances.toml"));
        let r: Result<()> = store.update(|c| {
            c.instances.push(sample_instance("work"));
            Err(Error::Cancelled)
        });
        assert!(r.is_err());
        assert!(!store.path().exists());
    }

    #[test]
    fn unversioned_file_is_migrated_with_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("instances.toml");
        let mut text = to_text(&Config { version: 1, instances: vec![sample_instance("work")] }).unwrap();
        text = text.replace("version = 1\n", "");
        assert!(!text.contains("version ="));
        fs::write(&path, &text).unwrap();
        let store = ConfigStore::new(&path);
        let cfg = store.load().unwrap();
        assert_eq!(cfg.instances.len(), 1);
        assert_eq!(fs::read_to_string(dir.path().join("instances.toml.v0.bak")).unwrap(), text);
        assert!(fs::read_to_string(&path).unwrap().contains("version = 1"));
    }

    #[test]
    fn newer_versions_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("instances.toml");
        fs::write(&path, "version = 99\n").unwrap();
        let err = ConfigStore::new(&path).load().unwrap_err();
        assert!(matches!(err, Error::ConfigTooNew { found: 99, .. }));
        fs::write(&path, "version = \"one\"\n").unwrap();
        assert!(matches!(ConfigStore::new(&path).load(), Err(Error::Parse { .. })));
    }

    #[test]
    fn migration_chain_runs_in_order() {
        fn v0(t: &mut toml::Table) -> Result<()> {
            t.insert("trail".into(), toml::Value::String("0".into()));
            Ok(())
        }
        fn v1(t: &mut toml::Table) -> Result<()> {
            let prev = t["trail"].as_str().unwrap().to_string();
            t.insert("trail".into(), toml::Value::String(format!("{prev}1")));
            // Example of a structural change: rename a key.
            if let Some(v) = t.remove("old_key") {
                t.insert("new_key".into(), v);
            }
            Ok(())
        }
        let mut table: toml::Table = toml::from_str("old_key = 5\n").unwrap();
        let found = migrate_table(&mut table, Path::new("x"), 2, &[v0, v1]).unwrap();
        assert_eq!(found, 0);
        assert_eq!(table["trail"].as_str(), Some("01"));
        assert_eq!(table["new_key"].as_integer(), Some(5));
        assert_eq!(table["version"].as_integer(), Some(2));
        let mut t: toml::Table = toml::from_str("version = 1\n").unwrap();
        assert!(migrate_table(&mut t, Path::new("x"), 3, &[v0]).is_err());
    }
}
