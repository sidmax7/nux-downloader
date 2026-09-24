//! `nuxd-testkit`: a fault-injecting HTTP origin for nuxd's integration
//! tests, and the deterministic content it serves. Dev-only — never
//! shipped, never depended on by `nuxd-core`, `cli` or `src-tauri`
//! (`ARCHITECTURE.md` §18, §20).
//!
//! An idea from Plexo's own e2e origin (`e2e/origin.ts`): a server that
//! can be told, per request, to misbehave in exactly the ways a real
//! server or network does, so a test can provoke a specific failure on
//! demand instead of guessing with timeouts.

mod content;
mod fault;
mod server;

pub use content::{content_byte, content_range};
pub use fault::Fault;
pub use server::{LoggedRequest, Origin, OriginOptions, RequestedRange};
