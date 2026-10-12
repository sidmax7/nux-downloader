//! The manifest loader on arbitrary bytes: errors are fine, panics and
//! non-round-tripping successes are not.
#![no_main]

use libfuzzer_sys::fuzz_target;
use nuxd_core::storage::manifest::{PackageManifest, QueueManifest};

fuzz_target!(|data: &[u8]| {
    if let Ok(manifest) = serde_json::from_slice::<PackageManifest>(data) {
        let again = serde_json::to_vec(&manifest).expect("a loaded manifest serializes");
        let back: PackageManifest = serde_json::from_slice(&again).expect("and reads back");
        assert_eq!(
            serde_json::to_vec(&back).unwrap(),
            again,
            "serialization is not stable"
        );
    }
    if let Ok(queue) = serde_json::from_slice::<QueueManifest>(data) {
        let again = serde_json::to_vec(&queue).expect("a loaded queue serializes");
        serde_json::from_slice::<QueueManifest>(&again).expect("and reads back");
    }
});
