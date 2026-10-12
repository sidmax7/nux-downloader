//! Multi-part archive grouping (`ARCHITECTURE.md` §12.1). Pure: names in,
//! groups out, no I/O.
//!
//! Volumes are numbered by their 1-based position in the set, whatever the
//! scheme calls them (`x.rar` is volume 1 of a legacy RAR set, `x.r00` is
//! volume 2), so "volume 7 missing" means the same thing everywhere.

use crate::model::{ArchiveScheme, ArchiveSet};
use std::collections::{BTreeSet, HashMap};

/// One group of the input names. `archive` is `None` for the group of
/// loose files that belong to no archive set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeGroup {
    pub archive: Option<ArchiveSet>,
    /// Indices into the input slice, in volume order for archives and in
    /// input order for the loose group.
    pub members: Vec<usize>,
    /// 1-based volume numbers absent from the set, ascending.
    pub missing: Vec<u32>,
}

impl VolumeGroup {
    /// More than one volume present or expected: a real "parts" set, as
    /// opposed to a lone `.zip`/`.rar`/`.7z`.
    #[must_use]
    pub fn is_multipart(&self) -> bool {
        self.archive.is_some() && (self.members.len() > 1 || !self.missing.is_empty())
    }
}

/// Where a volume sits in its set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Position {
    Numbered(u32),
    /// Split ZIP's final volume, `x.zip`, which follows every `x.zNN`.
    Last,
}

struct Parsed {
    /// Case-insensitive grouping key.
    key: String,
    base_name: String,
    scheme: ArchiveScheme,
    position: Position,
}

/// Groups `names` (file names, not paths). Archive sets come out in the
/// order their first member appears; all loose files form one group at the
/// position of the first loose file.
#[must_use]
pub fn group_names(names: &[&str]) -> Vec<VolumeGroup> {
    struct Set {
        base_name: String,
        scheme: ArchiveScheme,
        members: Vec<(Position, usize)>,
    }
    let mut sets: Vec<Set> = Vec::new();
    let mut index: HashMap<(String, ArchiveScheme), usize> = HashMap::new();
    // Slot order: Some(set index) or None for the loose group.
    let mut order: Vec<Option<usize>> = Vec::new();
    let mut loose: Vec<usize> = Vec::new();
    let mut seen_loose = false;

    for (i, name) in names.iter().enumerate() {
        match parse(name) {
            Some(p) => {
                let slot = *index.entry((p.key, p.scheme)).or_insert_with(|| {
                    sets.push(Set {
                        base_name: p.base_name,
                        scheme: p.scheme,
                        members: Vec::new(),
                    });
                    order.push(Some(sets.len() - 1));
                    sets.len() - 1
                });
                sets[slot].members.push((p.position, i));
            }
            None => {
                if !seen_loose {
                    seen_loose = true;
                    order.push(None);
                }
                loose.push(i);
            }
        }
    }

    // A "generic split" needs evidence: `.001` present or a second part.
    // Otherwise `report.2020`-style names would be mistaken for volumes.
    let mut demoted: Vec<usize> = Vec::new();
    for (s, set) in sets.iter().enumerate() {
        if set.scheme == ArchiveScheme::GenericSplit {
            let has_first = set.members.iter().any(|(p, _)| *p == Position::Numbered(1));
            if !has_first && set.members.len() < 2 {
                demoted.push(s);
            }
        }
    }
    for &s in &demoted {
        for &(_, i) in &sets[s].members {
            loose.push(i);
        }
        sets[s].members.clear();
        if !seen_loose {
            seen_loose = true;
            // Keep the loose group where the demoted set stood.
            if let Some(slot) = order.iter().position(|o| *o == Some(s)) {
                order[slot] = None;
            }
        }
    }
    loose.sort_unstable();

    let mut out = Vec::new();
    for slot in order {
        match slot {
            None => out.push(VolumeGroup {
                archive: None,
                members: loose.clone(),
                missing: Vec::new(),
            }),
            Some(s) if sets[s].members.is_empty() => {}
            Some(s) => {
                let set = &mut sets[s];
                set.members.sort();
                let present: BTreeSet<u32> = set
                    .members
                    .iter()
                    .filter_map(|(p, _)| match p {
                        Position::Numbered(n) => Some(*n),
                        Position::Last => None,
                    })
                    .collect();
                let has_last = set.members.iter().any(|(p, _)| *p == Position::Last);
                let missing = missing_volumes(set.scheme, &present, has_last);
                out.push(VolumeGroup {
                    archive: Some(ArchiveSet {
                        scheme: set.scheme,
                        base_name: set.base_name.clone(),
                    }),
                    members: set.members.iter().map(|&(_, i)| i).collect(),
                    missing,
                });
            }
        }
    }
    out
}

/// Gaps below the highest present volume, plus the volumes a scheme
/// implies (a split ZIP's `x.zip`, a legacy RAR's first volume).
fn missing_volumes(scheme: ArchiveScheme, present: &BTreeSet<u32>, has_last: bool) -> Vec<u32> {
    let Some(&max) = present.last() else {
        return Vec::new();
    };
    // Gaps between the volumes that are there. A name like
    // `x.part4294967295.rar` must not allocate four billion entries, so
    // the list is capped — enough to tell the user what is missing.
    const MAX_REPORTED: usize = 1000;
    let mut missing: Vec<u32> = Vec::new();
    let mut expected = 1u32;
    'gaps: for &have in present {
        while expected < have {
            if missing.len() >= MAX_REPORTED {
                break 'gaps;
            }
            missing.push(expected);
            expected += 1;
        }
        expected = have.saturating_add(1);
    }
    if scheme == ArchiveScheme::SplitZip && !has_last && missing.len() < MAX_REPORTED {
        missing.push(max.saturating_add(1));
    }
    missing
}

fn parse(name: &str) -> Option<Parsed> {
    // ASCII lowercasing keeps byte offsets identical to `name`.
    let lower = name.to_ascii_lowercase();
    let make = |stem: &str, scheme, position| Parsed {
        key: stem.to_string(),
        base_name: name[..stem.len()].to_string(),
        scheme,
        position,
    };

    if let Some(stem) = lower.strip_suffix(".rar") {
        if let Some((before, digits)) = stem.rsplit_once(".part")
            && let Some(n) = number(digits)
        {
            return Some(make(before, ArchiveScheme::Rar, Position::Numbered(n)));
        }
        return Some(make(stem, ArchiveScheme::RarLegacy, Position::Numbered(1)));
    }
    let (stem, tail) = lower.rsplit_once('.')?;
    // x.r00, x.r01, ...: volume 2 onwards of a legacy RAR set.
    if let Some(digits) = tail.strip_prefix('r')
        && digits.len() >= 2
        && let Some(n) = number(digits)
    {
        return Some(make(
            stem,
            ArchiveScheme::RarLegacy,
            Position::Numbered(n + 2),
        ));
    }
    // x.z01, x.z02, ...: split ZIP, with x.zip as the final volume.
    if let Some(digits) = tail.strip_prefix('z')
        && digits.len() >= 2
        && let Some(n) = number(digits)
    {
        return Some(make(stem, ArchiveScheme::SplitZip, Position::Numbered(n)));
    }
    if tail == "zip" {
        return Some(make(stem, ArchiveScheme::SplitZip, Position::Last));
    }
    if tail == "7z" {
        // The group key and display name keep the `.7z`.
        return Some(make(&lower, ArchiveScheme::SevenZip, Position::Numbered(1)));
    }
    // x.7z.001 / x.001: three or more digits.
    if tail.len() >= 3
        && let Some(n) = number(tail)
    {
        let scheme = if stem.ends_with(".7z") {
            ArchiveScheme::SevenZip
        } else {
            ArchiveScheme::GenericSplit
        };
        return Some(make(stem, scheme, Position::Numbered(n)));
    }
    None
}

/// A decimal number, or `None` for empty, signed or non-digit input.
fn number(digits: &str) -> Option<u32> {
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(names: &[&str]) -> Vec<VolumeGroup> {
        group_names(names)
    }

    fn archive(g: &VolumeGroup) -> (ArchiveScheme, &str) {
        let a = g.archive.as_ref().unwrap_or_else(|| panic!("loose group"));
        (a.scheme, a.base_name.as_str())
    }

    #[test]
    fn rar_part_volumes_group_and_order() {
        let g = run(&["game.part2.rar", "game.part1.rar", "game.part3.rar"]);
        assert_eq!(g.len(), 1);
        assert_eq!(archive(&g[0]), (ArchiveScheme::Rar, "game"));
        assert_eq!(g[0].members, vec![1, 0, 2]);
        assert!(g[0].missing.is_empty());
        assert!(g[0].is_multipart());
    }

    #[test]
    fn zero_padded_part_numbers_work() {
        let g = run(&["x.part01.rar", "x.part02.rar", "x.part10.rar"]);
        assert_eq!(g[0].members, vec![0, 1, 2]);
        assert_eq!(g[0].missing, vec![3, 4, 5, 6, 7, 8, 9]);
    }

    #[test]
    fn legacy_rar_orders_rar_then_r00() {
        let g = run(&["x.r01", "x.rar", "x.r00"]);
        assert_eq!(archive(&g[0]), (ArchiveScheme::RarLegacy, "x"));
        assert_eq!(g[0].members, vec![1, 2, 0]);
        assert!(g[0].missing.is_empty());
    }

    #[test]
    fn legacy_rar_without_its_first_volume_is_flagged() {
        let g = run(&["x.r00", "x.r01"]);
        assert_eq!(g[0].missing, vec![1]);
    }

    #[test]
    fn seven_zip_split() {
        let g = run(&["a.7z.002", "a.7z.001", "a.7z.003"]);
        assert_eq!(archive(&g[0]), (ArchiveScheme::SevenZip, "a.7z"));
        assert_eq!(g[0].members, vec![1, 0, 2]);
    }

    #[test]
    fn split_zip_reads_the_zip_last() {
        let g = run(&["x.zip", "x.z02", "x.z01"]);
        assert_eq!(archive(&g[0]), (ArchiveScheme::SplitZip, "x"));
        assert_eq!(g[0].members, vec![2, 1, 0]);
        assert!(g[0].missing.is_empty());
    }

    #[test]
    fn split_zip_missing_final_volume_is_flagged() {
        let g = run(&["x.z01", "x.z02"]);
        assert_eq!(g[0].missing, vec![3]);
    }

    #[test]
    fn generic_split() {
        let g = run(&["movie.mkv.002", "movie.mkv.001"]);
        assert_eq!(archive(&g[0]), (ArchiveScheme::GenericSplit, "movie.mkv"));
        assert_eq!(g[0].members, vec![1, 0]);
    }

    #[test]
    fn gaps_are_flagged_not_guessed() {
        let g = run(&["x.part1.rar", "x.part2.rar", "x.part4.rar", "x.part7.rar"]);
        assert_eq!(g[0].missing, vec![3, 5, 6]);
        let g = run(&["a.7z.001", "a.7z.003"]);
        assert_eq!(g[0].missing, vec![2]);
    }

    #[test]
    fn lone_archives_are_archives_but_not_multipart() {
        for (name, scheme) in [
            ("a.rar", ArchiveScheme::RarLegacy),
            ("a.zip", ArchiveScheme::SplitZip),
            ("a.7z", ArchiveScheme::SevenZip),
        ] {
            let g = run(&[name]);
            assert_eq!(g[0].archive.as_ref().map(|a| a.scheme), Some(scheme));
            assert!(!g[0].is_multipart(), "{name}");
        }
    }

    #[test]
    fn matching_is_case_insensitive_and_keeps_the_original_case() {
        let g = run(&["Game.PART1.RAR", "game.part2.rar"]);
        assert_eq!(g.len(), 1);
        assert_eq!(archive(&g[0]).1, "Game");
    }

    #[test]
    fn different_bases_make_different_groups_in_first_seen_order() {
        let g = run(&["b.part1.rar", "a.part1.rar", "b.part2.rar"]);
        assert_eq!(g.len(), 2);
        assert_eq!(archive(&g[0]).1, "b");
        assert_eq!(g[0].members, vec![0, 2]);
        assert_eq!(archive(&g[1]).1, "a");
    }

    #[test]
    fn mixed_batches_keep_loose_files_together() {
        let g = run(&["readme.txt", "x.part1.rar", "notes.pdf", "x.part2.rar"]);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].archive, None);
        assert_eq!(g[0].members, vec![0, 2]);
        assert_eq!(archive(&g[1]).1, "x");
    }

    #[test]
    fn same_base_in_different_schemes_stays_separate() {
        let g = run(&["x.part1.rar", "x.zip"]);
        assert_eq!(g.len(), 2);
    }

    #[test]
    fn ambiguous_numeric_extensions_are_not_volumes() {
        // A year or a lone number is not a split part.
        let g = run(&["report.2020", "backup.123", "data.01"]);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].archive, None);
        assert_eq!(g[0].members, vec![0, 1, 2]);
        // ...but two volumes, or a `.001`, is.
        assert!(run(&["backup.001"])[0].archive.is_some());
        assert!(run(&["backup.123", "backup.124"])[0].archive.is_some());
    }

    #[test]
    fn dotted_base_names_survive() {
        let g = run(&["my.show.s01.part1.rar", "my.show.s01.part2.rar"]);
        assert_eq!(archive(&g[0]).1, "my.show.s01");
    }

    #[test]
    fn duplicates_do_not_hide_a_gap() {
        let g = run(&["x.part1.rar", "x.part1.rar", "x.part3.rar"]);
        assert_eq!(g[0].missing, vec![2]);
        assert_eq!(g[0].members.len(), 3);
    }

    #[test]
    fn empty_and_non_ascii_inputs() {
        assert!(run(&[]).is_empty());
        let g = run(&["Ünï.part1.rar", "Ünï.part2.rar"]);
        assert_eq!(archive(&g[0]).1, "Ünï");
    }

    #[test]
    fn an_absurd_volume_number_cannot_exhaust_memory() {
        // Found by fuzzing: this once tried to list four billion gaps.
        let g = run(&["x.part4294967295.rar", "x.part1.rar"]);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].members.len(), 2);
        assert_eq!(g[0].missing.len(), 1000);
        assert_eq!(g[0].missing[0], 2);
        let g = run(&["a.7z.4294967295"]);
        assert!(g[0].missing.len() <= 1000);
    }
}
