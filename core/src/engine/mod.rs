//! The engine runtime: functional core, imperative shell
//! (`ARCHITECTURE.md` §14). `EngineState::handle` is the pure core; the
//! driver (P2.7) is the imperative shell that executes its effects and
//! feeds results back in as `Input`s. `sim` is the Rust counterpart of
//! Plexo's `simDownload`: a scripted harness that drives `handle`
//! directly, with fake attempts instead of real network I/O.

mod effect;
mod input;
pub mod sim;
mod snapshot;
mod state;

pub use effect::{Effect, EngineEvent, FilePrep, PreparedFile};
pub use input::{AbortReason, AttemptOutcome, Command, Input};
pub use snapshot::{EngineSnapshot, LinkStats};
pub use state::EngineState;
