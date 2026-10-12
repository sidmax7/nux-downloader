//! Multi-part archive grouping over arbitrary file-name lists.
#![no_main]

use libfuzzer_sys::fuzz_target;
use nuxd_core::group::group_names;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let names: Vec<&str> = text.split('\n').collect();
    let groups = group_names(&names);

    // Every input appears in exactly one group.
    let mut seen = vec![0u32; names.len()];
    for group in &groups {
        for &index in &group.members {
            assert!(index < names.len());
            seen[index] += 1;
        }
        assert!(group.missing.windows(2).all(|w| w[0] < w[1]), "missing not ascending");
        assert!(!group.members.is_empty());
    }
    assert!(seen.iter().all(|&n| n == 1), "{names:?} -> {groups:?}");
    // At most one group of loose files.
    assert!(groups.iter().filter(|g| g.archive.is_none()).count() <= 1);
});
