//! `nuxd-core`: the Nux Downloader engine. No UI or Tauri types live here —
//! see `ARCHITECTURE.md` for the design this crate is being built towards.

mod api;
pub mod collect;
pub mod config;
pub mod diagnostics;
pub mod driver;
pub mod engine;
pub mod error;
pub mod extract;
pub mod group;
pub mod limit;
pub mod model;
pub mod net;
pub mod plan;
pub mod probe;
pub mod resolve;
pub mod sched;
pub mod storage;
pub mod verify;
pub mod version;

pub use api::{AddOptions, Engine, Target};
pub use collect::{DraftFile, PackageDraft};
