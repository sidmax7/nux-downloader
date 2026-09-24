//! Network layer (`ARCHITECTURE.md` §6): HTTP client construction today
//! (Phase 2); interface discovery, per-link binding and health tracking
//! join this module in Phase 3.

pub mod bind;
pub mod client;
pub mod discover;
pub mod health;
pub mod label;
pub mod monitor;
