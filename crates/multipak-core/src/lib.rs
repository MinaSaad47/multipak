//! # multipak-core
//!
//! Library behind [Multipak](https://github.com/MinaSaad47/multipak): create and run
//! multiple isolated instances of the same Flatpak app, each with its own data folder.
//!
//! The crate contains no GUI code. Everything that touches the outside world goes
//! through the [`Host`] trait (running commands, reading `/proc`), so the whole
//! library can be tested with [`fake::FakeHost`] and a temporary `HOME`.
//!
//! The main entry point is [`Manager`].

pub mod apptype;
pub mod atomic;
pub mod backup;
pub mod capture;
pub mod config;
pub mod desktop;
pub mod doctor;
pub mod error;
pub mod exec;
pub mod fake;
pub mod flatpak;
pub mod fsutil;
pub mod icons;
pub mod instance;
pub mod keyfile;
pub mod launch;
pub mod manager;
pub mod paths;
pub mod transfer;

pub use apptype::{AppTypeTable, Detection, SingleInstanceRisk, Strategy};
pub use backup::{CancelToken, Progress};
pub use config::{Config, ConfigStore};
pub use doctor::{Fix, Issue, IssueKind};
pub use error::{Error, Result};
pub use exec::{CmdOutput, CommandSpec, Host, LaunchOutcome, SystemHost};
pub use flatpak::{FlatpakApp, PsEntry};
pub use instance::{Badge, Instance, InstanceDraft, InstanceKey};
pub use manager::{AppDetails, Manager, RunningInstance};
pub use paths::Paths;
