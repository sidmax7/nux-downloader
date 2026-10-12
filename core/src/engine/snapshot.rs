//! A read-only view of the engine's current state (`ARCHITECTURE.md`
//! §15's `Engine::snapshot`). Not part of the pure `handle()` pipeline —
//! the driver answers a snapshot request directly from its own
//! `EngineState`, without going through `Input`/`Effect` at all, since a
//! query changes nothing.

use crate::model::{Link, Package};
use crate::net::health::LinkState;

/// A cheap, point-in-time copy of everything a UI or CLI needs to render
/// the queue: the packages, in queue order, plus the id counters
/// (`queue.json`'s own persisted fields — a shell that also owns
/// `queue.json`, like the `Engine` facade, uses these to keep it current
/// without reaching into `EngineState` internals).
#[derive(Debug, Clone)]
pub struct EngineSnapshot {
    pub packages: Vec<Package>,
    pub next_package_id: u64,
    pub next_file_id: u64,
    /// The links the engine is currently scheduling onto, with their
    /// health and throughput.
    pub links: Vec<LinkStats>,
    /// Bytes per second of each file that is downloading, measured by the
    /// engine over the last few seconds on its own clock (Decision 34).
    /// A file not listed reads 0.
    pub file_speeds: std::collections::HashMap<crate::model::FileId, f64>,
}

/// One link's live numbers.
#[derive(Debug, Clone)]
pub struct LinkStats {
    pub link: Link,
    pub state: LinkState,
    /// Attempts currently running on this link.
    pub active: u32,
    /// Durable bytes this link has delivered since the engine started.
    pub bytes: u64,
    /// Bytes per second over the last few seconds (Decision 34).
    pub speed: f64,
}
