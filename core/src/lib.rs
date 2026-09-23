//! `nuxd-core`: the Nux Downloader engine. No UI or Tauri types live here —
//! see `ARCHITECTURE.md` for the design this crate is being built towards.

pub mod config;
pub mod error;
pub mod model;
pub mod plan;
pub mod probe;
pub mod storage;
pub mod version;
