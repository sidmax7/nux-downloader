//! Newtype identifiers. Each wraps a `u64` assigned by a monotonic counter
//! persisted in `queue.json` (see `ARCHITECTURE.md` §9.4) — the engine
//! owns the counters; this module only defines the types.

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! id_type {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(u64);

        impl $name {
            /// Wraps a raw id. Callers are expected to source these from a
            /// counter (or a stored manifest), not invent them ad hoc.
            #[must_use]
            pub const fn new(raw: u64) -> Self {
                Self(raw)
            }

            #[must_use]
            pub const fn get(self) -> u64 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

id_type!(
    PackageId,
    "Identifies one `Package` for the lifetime of the app."
);
id_type!(FileId, "Identifies one `FileEntry` within its package.");
id_type!(
    LinkId,
    "Identifies one network `Link` — stable across a rescan."
);
id_type!(
    AttemptId,
    "Identifies one in-flight transfer attempt. Runtime-only, never persisted."
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_json() {
        let id = PackageId::new(42);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "42");
        let back: PackageId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn distinct_id_types_are_not_interchangeable() {
        // This is a compile-time property, not a runtime one: the point of
        // newtypes is that `PackageId::new(1) == FileId::new(1)` does not
        // even typecheck. Nothing to assert at runtime beyond construction.
        let _package = PackageId::new(1);
        let _file = FileId::new(1);
    }
}
