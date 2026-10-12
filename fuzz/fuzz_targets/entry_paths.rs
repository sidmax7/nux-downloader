//! Archive entry names must never escape the output directory.
#![no_main]

use libfuzzer_sys::fuzz_target;
use nuxd_core::extract::sink::entry_components;

fuzz_target!(|data: &[u8]| {
    let name = String::from_utf8_lossy(data);
    for windows in [false, true] {
        if let Ok(parts) = entry_components(&name, windows) {
            for part in &parts {
                assert!(!part.is_empty());
                assert!(part != "." && part != "..", "{name:?} -> {parts:?}");
                assert!(!part.contains('/') && !part.contains('\\') && !part.contains('\0'));
                if windows {
                    assert!(!part.ends_with('.') && !part.ends_with(' '), "{part:?}");
                    assert!(!part.contains([':', '*', '?', '"', '<', '>', '|']), "{part:?}");
                }
            }
            // Joining onto a root can never leave it.
            let mut path = std::path::PathBuf::from("/root");
            path.extend(&parts);
            assert!(path.starts_with("/root"));
            assert!(path.components().all(|c| !matches!(c, std::path::Component::ParentDir)));
        }
    }
});
