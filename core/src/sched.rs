//! Scheduling (`ARCHITECTURE.md` §7.2): `pick_work` decides what a free
//! slot on a link should do next. Pure — no I/O, no clock — the same
//! "best of both worlds" assignment policy for one file or one thousand:
//! a single active file falls through to pass 2 and gets every slot;
//! many small single-block files each take one slot; a mix gets both at
//! once. Hedging (pass 3, §7.5) is a later phase (P7.1); this only
//! implements passes 1 and 2 plus the avoid rule.

use crate::config::{EngineConfig, HostPolicy};
use crate::model::{ByteRange, FileId, LinkId, Priority};
use std::collections::HashSet;

/// One block still worth an attempt: not currently held by any in-flight
/// attempt (the caller filters those out before building a `View` — a
/// block with an attempt already running isn't "pending").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateBlock {
    pub index: u32,
    /// The block's full extent, as laid out by `plan::block_range`.
    pub range: ByteRange,
    /// Bytes already durable from the block's start (`PartStatus::durable`).
    /// Resuming requests only the remainder.
    pub durable: u64,
    /// Links whose most recent attempt on this exact block delivered zero
    /// bytes (§7.2's avoid rule). Skipped while another link could take
    /// it instead; taken anyway rather than left stranded.
    pub avoid_links: Vec<LinkId>,
}

impl CandidateBlock {
    /// The range still left to request: `range` advanced past `durable`
    /// bytes already secured. `None` if `durable` has already consumed
    /// the whole (bounded) block — the caller shouldn't have offered a
    /// finished block as a candidate, but this stays defensive rather
    /// than trusting that invariant.
    #[must_use]
    pub fn remaining_range(&self) -> Option<ByteRange> {
        let start = self.range.start.checked_add(self.durable)?;
        match self.range.end {
            Some(end) if start <= end => ByteRange::bounded(start, end),
            Some(_) => None,
            None => Some(ByteRange::open_ended(start)),
        }
    }
}

/// One file with work available, and everything `pick_work` needs to
/// weigh it against every other file: `ARCHITECTURE.md` §4's queue order
/// (priority, then position) and §7.1's per-host, per-file limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileCandidate {
    pub file: FileId,
    /// The remote host — `HostPolicy::limits_for` and the per-host active
    /// counts below are keyed on this.
    pub host: String,
    pub priority: Priority,
    /// Position in `queue.json`; lower sorts earlier. Breaks ties between
    /// files of equal priority.
    pub queue_position: u64,
    /// Pending blocks, in any order — `pick_work` always prefers the
    /// lowest index (§7.2: "within a file, take the lowest-index pending
    /// block").
    pub blocks: Vec<CandidateBlock>,
}

/// One attempt currently in flight, for counting against the limits in
/// `ARCHITECTURE.md` §7.1. Not the full attempt state the driver tracks —
/// just what scheduling needs to see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveAttempt<'a> {
    pub file: FileId,
    pub link: LinkId,
    pub host: &'a str,
}

/// Everything `pick_work` reads to fill one free slot. Built fresh by
/// the caller (the engine, from its own state) on every call — this
/// borrows rather than owns, since a view is thrown away the instant
/// `pick_work` returns.
#[derive(Debug, Clone, Copy)]
pub struct SchedulerView<'a> {
    /// Active files with at least one pending block. Order doesn't
    /// matter — `pick_work` sorts by priority then `queue_position`.
    pub files: &'a [FileCandidate],
    /// Every attempt currently in flight, on any link.
    pub active: &'a [ActiveAttempt<'a>],
    pub hosts: &'a HostPolicy,
    pub config: &'a EngineConfig,
}

/// What a free slot should do. Only `Primary` exists until hedging lands
/// (P7.1 adds a `Hedge` variant).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Work {
    Primary {
        file: FileId,
        block: u32,
        /// The range to actually request — already advanced past any
        /// durable prefix (`CandidateBlock::remaining_range`).
        range: ByteRange,
        /// Where in the destination file the response's first byte
        /// lands. Always equal to `range.start`: in-place positional
        /// writes (§9.1) mean there is no separate offset to track, but
        /// this mirrors `Effect::StartAttempt`'s shape (§14.1) exactly.
        file_offset: u64,
    },
}

fn active_count(
    active: &[ActiveAttempt<'_>],
    predicate: impl Fn(&ActiveAttempt<'_>) -> bool,
) -> u32 {
    u32::try_from(active.iter().filter(|attempt| predicate(attempt)).count()).unwrap_or(u32::MAX)
}

/// One assignment pass over every candidate file, in priority/position
/// order. `enforce_soft_cap` distinguishes pass 1 (spread) from pass 2
/// (focus); `respect_avoid` is dropped for a final fallback so the avoid
/// rule can never strand a block with nothing else to try.
fn pick_pass(
    view: &SchedulerView<'_>,
    link: LinkId,
    enforce_soft_cap: bool,
    respect_avoid: bool,
) -> Option<Work> {
    let mut files: Vec<&FileCandidate> = view.files.iter().collect();
    files.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then(a.queue_position.cmp(&b.queue_position))
    });

    for file in files {
        if enforce_soft_cap {
            let file_active = active_count(view.active, |a| a.file == file.file);
            if file_active >= view.config.soft_max_connections_per_file {
                continue;
            }
        }

        let limits = view.hosts.limits_for(&file.host);
        let host_link_active = active_count(view.active, |a| a.host == file.host && a.link == link);
        if host_link_active >= limits.max_connections_per_link {
            continue;
        }

        if let Some(max_active_files) = limits.max_active_files {
            let active_files_for_host: HashSet<FileId> = view
                .active
                .iter()
                .filter(|a| a.host == file.host)
                .map(|a| a.file)
                .collect();
            let already_active = active_files_for_host.contains(&file.file);
            if !already_active
                && u32::try_from(active_files_for_host.len()).unwrap_or(u32::MAX)
                    >= max_active_files
            {
                continue;
            }
        }

        let mut blocks: Vec<&CandidateBlock> = file.blocks.iter().collect();
        blocks.sort_by_key(|b| b.index);
        for block in blocks {
            if respect_avoid && block.avoid_links.contains(&link) {
                continue;
            }
            let Some(range) = block.remaining_range() else {
                continue;
            };
            return Some(Work::Primary {
                file: file.file,
                block: block.index,
                file_offset: range.start,
                range,
            });
        }
    }
    None
}

/// Fills one free slot on `link`: pass 1 (spread, soft per-file cap
/// enforced, avoid rule respected), then pass 2 (focus, soft cap
/// dropped), then — only if that still finds nothing — pass 2 again
/// with the avoid rule dropped too, so a block avoided on every link
/// still gets taken rather than left stranded. `None` means the slot
/// stays free until the next state change.
#[must_use]
pub fn pick_work(view: &SchedulerView<'_>, link: LinkId) -> Option<Work> {
    pick_pass(view, link, true, true)
        .or_else(|| pick_pass(view, link, false, true))
        .or_else(|| pick_pass(view, link, false, false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HostLimits;

    fn block(index: u32, start: u64, end: u64) -> CandidateBlock {
        CandidateBlock {
            index,
            range: ByteRange::bounded(start, end).unwrap(),
            durable: 0,
            avoid_links: Vec::new(),
        }
    }

    fn file(
        id: u64,
        host: &str,
        priority: Priority,
        position: u64,
        blocks: Vec<CandidateBlock>,
    ) -> FileCandidate {
        FileCandidate {
            file: FileId::new(id),
            host: host.to_string(),
            priority,
            queue_position: position,
            blocks,
        }
    }

    fn default_view<'a>(
        files: &'a [FileCandidate],
        active: &'a [ActiveAttempt<'a>],
        hosts: &'a HostPolicy,
        config: &'a EngineConfig,
    ) -> SchedulerView<'a> {
        SchedulerView {
            files,
            active,
            hosts,
            config,
        }
    }

    #[test]
    fn a_single_file_gets_the_slot() {
        let files = [file(
            1,
            "example.com",
            Priority::Normal,
            0,
            vec![block(0, 0, 99)],
        )];
        let active = [];
        let hosts = HostPolicy::default();
        let config = EngineConfig::default();
        let view = default_view(&files, &active, &hosts, &config);

        let work = pick_work(&view, LinkId::new(1)).unwrap();
        assert_eq!(
            work,
            Work::Primary {
                file: FileId::new(1),
                block: 0,
                range: ByteRange::bounded(0, 99).unwrap(),
                file_offset: 0,
            }
        );
    }

    #[test]
    fn no_candidates_returns_none() {
        let files = [];
        let active = [];
        let hosts = HostPolicy::default();
        let config = EngineConfig::default();
        let view = default_view(&files, &active, &hosts, &config);

        assert_eq!(pick_work(&view, LinkId::new(1)), None);
    }

    #[test]
    fn resumes_from_the_durable_frontier() {
        let mut b = block(0, 0, 99);
        b.durable = 40;
        let files = [file(1, "example.com", Priority::Normal, 0, vec![b])];
        let active = [];
        let hosts = HostPolicy::default();
        let config = EngineConfig::default();
        let view = default_view(&files, &active, &hosts, &config);

        let work = pick_work(&view, LinkId::new(1)).unwrap();
        assert_eq!(
            work,
            Work::Primary {
                file: FileId::new(1),
                block: 0,
                range: ByteRange::bounded(40, 99).unwrap(),
                file_offset: 40,
            }
        );
    }

    #[test]
    fn within_a_file_the_lowest_index_pending_block_wins() {
        let files = [file(
            1,
            "example.com",
            Priority::Normal,
            0,
            vec![block(2, 200, 299), block(0, 0, 99), block(1, 100, 199)],
        )];
        let active = [];
        let hosts = HostPolicy::default();
        let config = EngineConfig::default();
        let view = default_view(&files, &active, &hosts, &config);

        let work = pick_work(&view, LinkId::new(1)).unwrap();
        assert!(matches!(work, Work::Primary { block: 0, .. }));
    }

    #[test]
    fn higher_priority_files_are_served_first() {
        let files = [
            file(
                1,
                "a.example.com",
                Priority::Normal,
                0,
                vec![block(0, 0, 99)],
            ),
            file(2, "b.example.com", Priority::High, 1, vec![block(0, 0, 99)]),
        ];
        let active = [];
        let hosts = HostPolicy::default();
        let config = EngineConfig::default();
        let view = default_view(&files, &active, &hosts, &config);

        let work = pick_work(&view, LinkId::new(1)).unwrap();
        assert!(matches!(work, Work::Primary { file, .. } if file == FileId::new(2)));
    }

    #[test]
    fn ties_break_by_earlier_queue_position() {
        let files = [
            file(
                1,
                "a.example.com",
                Priority::Normal,
                5,
                vec![block(0, 0, 99)],
            ),
            file(
                2,
                "b.example.com",
                Priority::Normal,
                1,
                vec![block(0, 0, 99)],
            ),
        ];
        let active = [];
        let hosts = HostPolicy::default();
        let config = EngineConfig::default();
        let view = default_view(&files, &active, &hosts, &config);

        let work = pick_work(&view, LinkId::new(1)).unwrap();
        assert!(matches!(work, Work::Primary { file, .. } if file == FileId::new(2)));
    }

    #[test]
    fn pass_1_spreads_across_files_once_the_soft_cap_is_hit() {
        let config = EngineConfig {
            soft_max_connections_per_file: 1,
            ..Default::default()
        };
        let files = [
            file(
                1,
                "a.example.com",
                Priority::Normal,
                0,
                vec![block(0, 0, 99)],
            ),
            file(
                2,
                "b.example.com",
                Priority::Normal,
                1,
                vec![block(0, 0, 99)],
            ),
        ];
        // File 1 already has a connection elsewhere — at its soft cap.
        let active = [ActiveAttempt {
            file: FileId::new(1),
            link: LinkId::new(99),
            host: "a.example.com",
        }];
        let hosts = HostPolicy::default();
        let view = default_view(&files, &active, &hosts, &config);

        let work = pick_work(&view, LinkId::new(1)).unwrap();
        assert!(matches!(work, Work::Primary { file, .. } if file == FileId::new(2)));
    }

    #[test]
    fn pass_2_ignores_the_soft_cap_when_nothing_else_has_work() {
        let config = EngineConfig {
            soft_max_connections_per_file: 1,
            ..Default::default()
        };
        // Only one file exists at all — it must still get the slot even
        // though it's already past its soft cap (a single file gets
        // every slot, like Plexo).
        let files = [file(
            1,
            "a.example.com",
            Priority::Normal,
            0,
            vec![block(0, 0, 99)],
        )];
        let active = [ActiveAttempt {
            file: FileId::new(1),
            link: LinkId::new(99),
            host: "a.example.com",
        }];
        let hosts = HostPolicy::default();
        let view = default_view(&files, &active, &hosts, &config);

        let work = pick_work(&view, LinkId::new(1)).unwrap();
        assert!(matches!(work, Work::Primary { file, .. } if file == FileId::new(1)));
    }

    #[test]
    fn a_hard_host_per_link_cap_is_never_exceeded() {
        let mut hosts = HostPolicy::default();
        hosts.add_rule(
            "a.example.com",
            HostLimits {
                max_connections_per_link: 1,
                max_active_files: None,
            },
        );
        let files = [file(
            1,
            "a.example.com",
            Priority::Normal,
            0,
            vec![block(0, 0, 99)],
        )];
        // Already one connection to this host on this exact link.
        let active = [ActiveAttempt {
            file: FileId::new(1),
            link: LinkId::new(1),
            host: "a.example.com",
        }];
        let config = EngineConfig::default();
        let view = default_view(&files, &active, &hosts, &config);

        assert_eq!(pick_work(&view, LinkId::new(1)), None);
    }

    #[test]
    fn a_host_per_link_cap_still_allows_a_different_link() {
        let mut hosts = HostPolicy::default();
        hosts.add_rule(
            "a.example.com",
            HostLimits {
                max_connections_per_link: 1,
                max_active_files: None,
            },
        );
        let files = [file(
            1,
            "a.example.com",
            Priority::Normal,
            0,
            vec![block(0, 0, 99)],
        )];
        let active = [ActiveAttempt {
            file: FileId::new(1),
            link: LinkId::new(1),
            host: "a.example.com",
        }];
        let config = EngineConfig::default();
        let view = default_view(&files, &active, &hosts, &config);

        let work = pick_work(&view, LinkId::new(2)).unwrap();
        assert!(matches!(work, Work::Primary { file, .. } if file == FileId::new(1)));
    }

    #[test]
    fn max_active_files_blocks_a_new_file_but_not_an_already_active_one() {
        let mut hosts = HostPolicy::default();
        hosts.add_rule(
            "a.example.com",
            HostLimits {
                max_connections_per_link: 8,
                max_active_files: Some(1),
            },
        );
        let files = [
            file(
                1,
                "a.example.com",
                Priority::Normal,
                0,
                vec![block(0, 0, 99)],
            ),
            file(
                2,
                "a.example.com",
                Priority::Normal,
                1,
                vec![block(0, 0, 99)],
            ),
        ];
        // File 1 is already the host's one active file.
        let active = [ActiveAttempt {
            file: FileId::new(1),
            link: LinkId::new(2),
            host: "a.example.com",
        }];
        let config = EngineConfig::default();
        let view = default_view(&files, &active, &hosts, &config);

        // File 2 is a new file for this host — blocked. File 1 can still
        // get more connections, since it's already the active one.
        let work = pick_work(&view, LinkId::new(1)).unwrap();
        assert!(matches!(work, Work::Primary { file, .. } if file == FileId::new(1)));
    }

    #[test]
    fn the_avoid_rule_skips_a_block_while_another_is_free() {
        let files = [file(
            1,
            "a.example.com",
            Priority::Normal,
            0,
            vec![
                CandidateBlock {
                    index: 0,
                    range: ByteRange::bounded(0, 99).unwrap(),
                    durable: 0,
                    avoid_links: vec![LinkId::new(1)],
                },
                block(1, 100, 199),
            ],
        )];
        let active = [];
        let hosts = HostPolicy::default();
        let config = EngineConfig::default();
        let view = default_view(&files, &active, &hosts, &config);

        let work = pick_work(&view, LinkId::new(1)).unwrap();
        assert!(matches!(work, Work::Primary { block: 1, .. }));
    }

    #[test]
    fn the_avoid_rule_is_dropped_rather_than_stranding_the_only_block() {
        let files = [file(
            1,
            "a.example.com",
            Priority::Normal,
            0,
            vec![CandidateBlock {
                index: 0,
                range: ByteRange::bounded(0, 99).unwrap(),
                durable: 0,
                avoid_links: vec![LinkId::new(1)],
            }],
        )];
        let active = [];
        let hosts = HostPolicy::default();
        let config = EngineConfig::default();
        let view = default_view(&files, &active, &hosts, &config);

        let work = pick_work(&view, LinkId::new(1)).unwrap();
        assert!(matches!(work, Work::Primary { block: 0, .. }));
    }

    #[test]
    fn many_single_block_files_each_take_one_slot() {
        // One slot request per file, independently — this just checks
        // that a second, unrelated file is unaffected by the first's
        // assignment (the driver would call `pick_work` once per free
        // slot; this simulates two such calls with file 1's block
        // already claimed, i.e. no longer a candidate).
        let files = [file(
            2,
            "b.example.com",
            Priority::Normal,
            1,
            vec![block(0, 0, 49)],
        )];
        let active = [ActiveAttempt {
            file: FileId::new(1),
            link: LinkId::new(1),
            host: "a.example.com",
        }];
        let hosts = HostPolicy::default();
        let config = EngineConfig::default();
        let view = default_view(&files, &active, &hosts, &config);

        let work = pick_work(&view, LinkId::new(2)).unwrap();
        assert!(matches!(work, Work::Primary { file, .. } if file == FileId::new(2)));
    }

    #[test]
    fn remaining_range_advances_past_durable_bytes() {
        let mut b = block(0, 0, 99);
        b.durable = 100;
        // Fully durable — nothing left to request.
        assert_eq!(b.remaining_range(), None);
    }

    #[test]
    fn remaining_range_on_an_open_ended_block_advances_the_start_only() {
        let b = CandidateBlock {
            index: 0,
            range: ByteRange::open_ended(0),
            durable: 4096,
            avoid_links: Vec::new(),
        };
        assert_eq!(b.remaining_range(), Some(ByteRange::open_ended(4096)));
    }
}
