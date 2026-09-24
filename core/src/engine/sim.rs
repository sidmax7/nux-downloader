//! A scripted simulator harness — the Rust counterpart of Plexo's
//! `simDownload` (`ARCHITECTURE.md` §14.2). Drives `EngineState::handle`
//! directly with hand-built `Input`s standing in for a driver's real
//! effects (fake transfers, fake probes, fake I/O), and checks the §4
//! invariants hold after every step. No sockets, no files, no tokio.

use super::{Effect, EngineEvent, EngineState};
use crate::error::ProbeError;
use crate::model::{Link, LinkId, LinkKind, PackageStatus};
#[cfg(test)]
use crate::net::health::LinkState;
use crate::probe::ProbeResult;
use std::net::Ipv4Addr;

/// Every §4 invariant that must hold after any `handle` call, for any
/// reachable state: one part per block, no part claiming more than its
/// block holds, and a `Done` package only when every file is `Verified`.
pub fn check_invariants(state: &EngineState) {
    for package in state.packages() {
        for file in &package.files {
            if let Some(layout) = file.layout {
                assert_eq!(
                    file.parts.len(),
                    layout.count as usize,
                    "file {:?}: one part per block",
                    file.id
                );
                let total = file.expected_size.unwrap_or(0);
                for (index, part) in file.parts.iter().enumerate() {
                    let block_index = u32::try_from(index).unwrap_or(u32::MAX);
                    if let Some(len) = crate::plan::block_len(&layout, total, block_index) {
                        assert!(
                            part.durable <= len,
                            "file {:?} block {index}: durable {} exceeds block length {len}",
                            file.id,
                            part.durable
                        );
                    }
                }
            }
        }
        if package.status == PackageStatus::Done {
            assert!(
                package.all_files_verified(),
                "package {:?}: Done but not every file is Verified",
                package.id
            );
        }
    }
}

/// One fake network link, usable in any scenario that doesn't care about
/// link identity beyond having exactly one.
#[must_use]
pub fn one_link() -> Link {
    Link {
        id: LinkId::new(1),
        name: "sim0".to_string(),
        label: "Simulated".to_string(),
        kind: LinkKind::Other,
        v4: Some(Ipv4Addr::LOCALHOST),
        v6: None,
        max_slots: 8,
        limit: None,
    }
}

/// A successful probe result for a small, splittable file of `size`
/// bytes at `url`.
pub fn ok_probe(url: &str, size: u64) -> Result<ProbeResult, ProbeError> {
    let parsed =
        url::Url::parse(url).unwrap_or_else(|error| panic!("sim: bad test URL {url}: {error}"));
    Ok(ProbeResult {
        final_url: parsed,
        suggested_name: "file.bin".to_string(),
        expected_size: Some(size),
        supports_ranges: true,
        version: crate::model::FileVersion {
            etag: Some("\"sim-etag\"".to_string()),
            last_modified: None,
            total_bytes: size,
        },
    })
}

/// Finds the `Effect::StartAttempt` for `file` in `effects`, if any —
/// scenarios use this to learn the attempt id the engine assigned so
/// they can later report a fake `AttemptEnded` for it.
#[must_use]
pub fn find_start_attempt(
    effects: &[Effect],
    file: crate::model::FileId,
) -> Option<(crate::model::AttemptId, crate::model::ByteRange)> {
    effects.iter().find_map(|effect| match effect {
        Effect::StartAttempt {
            id, file: f, range, ..
        } if *f == file => Some((*id, *range)),
        _ => None,
    })
}

#[must_use]
pub fn find_prepare(effects: &[Effect], file: crate::model::FileId) -> bool {
    effects
        .iter()
        .any(|effect| matches!(effect, Effect::PrepareFile { file: f, .. } if *f == file))
}

#[must_use]
pub fn package_added(effects: &[Effect]) -> Option<crate::model::PackageId> {
    effects.iter().find_map(|effect| match effect {
        Effect::Emit(EngineEvent::PackageAdded { package }) => Some(*package),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EngineConfig;
    use crate::engine::{AbortReason, AttemptOutcome, Command, Input, PreparedFile};
    use crate::error::TransferError;
    use crate::model::{FileStatus, Priority};
    use std::time::{Duration, Instant};

    fn add_single_file(
        state: &mut EngineState,
        now: Instant,
        url: &str,
    ) -> (crate::model::PackageId, crate::model::FileId) {
        let effects = state.handle(
            Input::Command(Command::AddPackage {
                name: "pkg".to_string(),
                dest_dir: std::path::PathBuf::from("/tmp/nuxd-sim"),
                sources: vec![url.to_string()],
                priority: Priority::Normal,
            }),
            now,
        );
        check_invariants(state);
        let package = package_added(&effects)
            .unwrap_or_else(|| panic!("sim: no PackageAdded in {effects:?}"));
        let file = state
            .package(package)
            .unwrap_or_else(|| panic!("sim: package not found"))
            .files[0]
            .id;
        (package, file)
    }

    /// Drives a freshly probed file all the way to `Active`, returning
    /// the started attempt's id and its requested range. Assumes exactly
    /// one block (a small, single-block file).
    fn activate_and_start(
        state: &mut EngineState,
        now: Instant,
        file: crate::model::FileId,
        url: &str,
        size: u64,
    ) -> (crate::model::AttemptId, crate::model::ByteRange) {
        state.handle(
            Input::ProbeDone {
                file,
                result: ok_probe(url, size),
            },
            now,
        );
        check_invariants(state);
        let effects = state.handle(
            Input::FilePrepared {
                file,
                result: Ok(PreparedFile {
                    final_name: "file.bin".to_string(),
                    frontiers_valid: true,
                }),
            },
            now,
        );
        check_invariants(state);
        find_start_attempt(&effects, file)
            .unwrap_or_else(|| panic!("sim: no StartAttempt in {effects:?}"))
    }

    #[test]
    fn happy_path_single_file_single_link() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![one_link()]), now);

        let (package, file) = add_single_file(&mut state, now, "http://example.com/f.bin");
        let (attempt, range) =
            activate_and_start(&mut state, now, file, "http://example.com/f.bin", 1000);
        assert_eq!(range.len(), Some(1000));

        state.handle(
            Input::Tick {
                progress: vec![(attempt, 500)],
            },
            now,
        );
        check_invariants(&state);
        assert_eq!(
            state.package(package).unwrap().files[0].status,
            FileStatus::Active
        );

        let effects = state.handle(
            Input::AttemptEnded {
                id: attempt,
                outcome: AttemptOutcome::Success,
                position: 1000,
            },
            now,
        );
        check_invariants(&state);

        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::Finalize { file: f } if *f == file))
        );
        let pkg = state.package(package).unwrap();
        assert_eq!(pkg.files[0].status, FileStatus::Verified);
        assert_eq!(pkg.status, PackageStatus::Done);
    }

    #[test]
    fn an_empty_file_completes_on_activation_with_no_attempt() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![one_link()]), now);

        let (package, file) = add_single_file(&mut state, now, "http://example.com/empty.bin");
        state.handle(
            Input::ProbeDone {
                file,
                result: ok_probe("http://example.com/empty.bin", 0),
            },
            now,
        );
        let effects = state.handle(
            Input::FilePrepared {
                file,
                result: Ok(PreparedFile {
                    final_name: "empty.bin".to_string(),
                    frontiers_valid: true,
                }),
            },
            now,
        );
        check_invariants(&state);

        assert!(find_start_attempt(&effects, file).is_none());
        let pkg = state.package(package).unwrap();
        assert_eq!(pkg.files[0].status, FileStatus::Verified);
        assert_eq!(pkg.status, PackageStatus::Done);
    }

    #[test]
    fn a_stall_backs_off_before_retrying() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![one_link()]), now);

        let (_package, file) = add_single_file(&mut state, now, "http://example.com/f.bin");
        let (attempt, _) =
            activate_and_start(&mut state, now, file, "http://example.com/f.bin", 1000);

        let effects = state.handle(
            Input::AttemptEnded {
                id: attempt,
                outcome: AttemptOutcome::Failed(TransferError::Stall),
                position: 200,
            },
            now,
        );
        check_invariants(&state);
        // Backed off — no immediate replacement attempt yet.
        assert!(find_start_attempt(&effects, file).is_none());
        assert!(effects.iter().any(|e| matches!(e, Effect::WakeAt(_))));
    }

    #[test]
    fn a_stall_eventually_retries_after_the_backoff_elapses() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![one_link()]), now);

        let (_package, file) = add_single_file(&mut state, now, "http://example.com/f.bin");
        let (attempt, _) =
            activate_and_start(&mut state, now, file, "http://example.com/f.bin", 1000);

        state.handle(
            Input::AttemptEnded {
                id: attempt,
                outcome: AttemptOutcome::Failed(TransferError::Stall),
                position: 200,
            },
            now,
        );

        let later = now + Duration::from_secs(2);
        let effects = state.handle(Input::Tick { progress: vec![] }, later);
        check_invariants(&state);
        let (new_attempt, range) = find_start_attempt(&effects, file)
            .unwrap_or_else(|| panic!("sim: expected a retry StartAttempt in {effects:?}"));
        assert_ne!(new_attempt, attempt);
        // Resumes from the 200 bytes already durable.
        assert_eq!(range.start, 200);
    }

    #[test]
    fn a_connect_failure_blames_the_link_and_retries_without_backoff() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![one_link()]), now);

        let (_package, file) = add_single_file(&mut state, now, "http://example.com/f.bin");
        let (attempt, _) =
            activate_and_start(&mut state, now, file, "http://example.com/f.bin", 1000);

        let effects = state.handle(
            Input::AttemptEnded {
                id: attempt,
                outcome: AttemptOutcome::Failed(TransferError::Connect("refused".to_string())),
                position: 0,
            },
            now,
        );
        check_invariants(&state);
        // Link blame: retried immediately, in the very same call.
        let (new_attempt, _) = find_start_attempt(&effects, file)
            .unwrap_or_else(|| panic!("sim: expected an immediate retry in {effects:?}"));
        assert_ne!(new_attempt, attempt);
    }

    #[test]
    fn too_many_retries_fails_the_file_and_the_package() {
        let config = EngineConfig {
            max_retries_per_block: 2,
            ..Default::default()
        };
        let mut state = EngineState::new(config);
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![one_link()]), now);

        let (package, file) = add_single_file(&mut state, now, "http://example.com/f.bin");
        let (mut attempt, _) =
            activate_and_start(&mut state, now, file, "http://example.com/f.bin", 1000);

        let mut clock = now;
        for _ in 0..3 {
            state.handle(
                Input::AttemptEnded {
                    id: attempt,
                    outcome: AttemptOutcome::Failed(TransferError::Stall),
                    position: 0,
                },
                clock,
            );
            clock += Duration::from_secs(30);
            let effects = state.handle(Input::Tick { progress: vec![] }, clock);
            if let Some((next, _)) = find_start_attempt(&effects, file) {
                attempt = next;
            }
        }
        check_invariants(&state);

        let pkg = state.package(package).unwrap();
        assert_eq!(
            pkg.files[0].status,
            FileStatus::Failed(crate::model::FileError::TooManyRetries)
        );
        assert_eq!(
            pkg.status,
            PackageStatus::Failed(crate::model::PackageError::FilesFailed)
        );
    }

    #[test]
    fn pause_aborts_in_flight_attempts_and_resume_restarts_them() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![one_link()]), now);

        let (package, file) = add_single_file(&mut state, now, "http://example.com/f.bin");
        let (attempt, _) =
            activate_and_start(&mut state, now, file, "http://example.com/f.bin", 1000);

        let effects = state.handle(Input::Command(Command::Pause(package)), now);
        check_invariants(&state);
        assert!(effects
            .iter()
            .any(|e| matches!(e, Effect::AbortAttempt { id, reason: AbortReason::Pause } if *id == attempt)));
        assert_eq!(
            state.package(package).unwrap().status,
            PackageStatus::Paused
        );
        assert_eq!(state.attempt_count(), 0);

        let effects = state.handle(Input::Command(Command::Resume(package)), now);
        check_invariants(&state);
        assert_eq!(
            state.package(package).unwrap().status,
            PackageStatus::Downloading
        );
        assert!(find_start_attempt(&effects, file).is_some());
    }

    #[test]
    fn cancel_aborts_attempts_and_fails_the_package() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![one_link()]), now);

        let (package, file) = add_single_file(&mut state, now, "http://example.com/f.bin");
        let (attempt, _) =
            activate_and_start(&mut state, now, file, "http://example.com/f.bin", 1000);

        let effects = state.handle(Input::Command(Command::Cancel(package)), now);
        check_invariants(&state);
        assert!(effects
            .iter()
            .any(|e| matches!(e, Effect::AbortAttempt { id, reason: AbortReason::Cancel } if *id == attempt)));
        assert!(matches!(
            state.package(package).unwrap().status,
            PackageStatus::Failed(_)
        ));
        let _ = file;
    }

    #[test]
    fn two_single_block_files_each_take_one_slot() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![one_link()]), now);

        let effects = state.handle(
            Input::Command(Command::AddPackage {
                name: "pkg".to_string(),
                dest_dir: std::path::PathBuf::from("/tmp/nuxd-sim"),
                sources: vec![
                    "http://a.example.com/f.bin".to_string(),
                    "http://b.example.com/f.bin".to_string(),
                ],
                priority: Priority::Normal,
            }),
            now,
        );
        let package = package_added(&effects).unwrap();
        let files: Vec<_> = state
            .package(package)
            .unwrap()
            .files
            .iter()
            .map(|f| f.id)
            .collect();

        state.handle(
            Input::ProbeDone {
                file: files[0],
                result: ok_probe("http://a.example.com/f.bin", 500),
            },
            now,
        );
        state.handle(
            Input::ProbeDone {
                file: files[1],
                result: ok_probe("http://b.example.com/f.bin", 500),
            },
            now,
        );
        state.handle(
            Input::FilePrepared {
                file: files[0],
                result: Ok(PreparedFile {
                    final_name: "f0.bin".to_string(),
                    frontiers_valid: true,
                }),
            },
            now,
        );
        let effects = state.handle(
            Input::FilePrepared {
                file: files[1],
                result: Ok(PreparedFile {
                    final_name: "f1.bin".to_string(),
                    frontiers_valid: true,
                }),
            },
            now,
        );
        check_invariants(&state);

        assert!(find_start_attempt(&effects, files[1]).is_some());
        assert_eq!(state.attempt_count(), 2);
    }

    #[test]
    fn an_invalid_url_fails_its_file_without_blocking_the_package() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![one_link()]), now);

        let effects = state.handle(
            Input::Command(Command::AddPackage {
                name: "pkg".to_string(),
                dest_dir: std::path::PathBuf::from("/tmp/nuxd-sim"),
                sources: vec![
                    "not a url".to_string(),
                    "http://example.com/f.bin".to_string(),
                ],
                priority: Priority::Normal,
            }),
            now,
        );
        let package = package_added(&effects).unwrap();
        let files: Vec<_> = state
            .package(package)
            .unwrap()
            .files
            .iter()
            .map(|f| f.id)
            .collect();
        assert_eq!(
            state.package(package).unwrap().files[0].status,
            FileStatus::Failed(crate::model::FileError::ProbeFailed {
                detail: "couldn't parse as a URL".to_string()
            })
        );

        let (_, good_file) = (files[0], files[1]);
        let (attempt, _) =
            activate_and_start(&mut state, now, good_file, "http://example.com/f.bin", 100);
        let effects = state.handle(
            Input::AttemptEnded {
                id: attempt,
                outcome: AttemptOutcome::Success,
                position: 100,
            },
            now,
        );
        check_invariants(&state);
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::Emit(EngineEvent::PackageFailed { .. })))
        );
        assert_eq!(
            state.package(package).unwrap().status,
            PackageStatus::Failed(crate::model::PackageError::FilesFailed)
        );
    }

    fn sample_package(file_status: FileStatus) -> crate::model::Package {
        let mut file = crate::model::FileEntry::new(
            crate::model::FileId::new(7),
            "http://example.com/f.bin",
            "f.bin",
        )
        .unwrap();
        file.url = Some(url::Url::parse("http://example.com/f.bin").unwrap());
        file.expected_size = Some(1000);
        file.supports_ranges = true;
        file.layout = Some(crate::model::BlockLayout {
            block_size: 1000,
            count: 1,
        });
        file.parts = vec![crate::model::PartStatus {
            durable: 500,
            link: None,
        }];
        file.status = file_status;

        let mut package = crate::model::Package::new(
            crate::model::PackageId::new(3),
            "pkg",
            std::path::PathBuf::from("/tmp/nuxd-sim"),
            vec![file],
        )
        .unwrap();
        package.status = PackageStatus::Downloading;
        package
    }

    fn sample_queue() -> crate::storage::manifest::QueueManifest {
        crate::storage::manifest::QueueManifest {
            schema_version: crate::storage::manifest::QUEUE_SCHEMA_VERSION,
            package_order: vec![crate::model::PackageId::new(3)],
            next_package_id: 4,
            next_file_id: 8,
            settings: crate::storage::manifest::QueueSettings::default(),
        }
    }

    #[test]
    fn restore_reopens_an_active_file_and_keeps_its_durable_frontier() {
        let package = sample_package(FileStatus::Active);
        let (state, effects) =
            EngineState::restore(EngineConfig::default(), &sample_queue(), vec![package]);

        let file = state
            .package(crate::model::PackageId::new(3))
            .unwrap()
            .files[0]
            .id;
        assert_eq!(
            state
                .package(crate::model::PackageId::new(3))
                .unwrap()
                .files[0]
                .status,
            FileStatus::Preparing
        );
        assert_eq!(
            state
                .package(crate::model::PackageId::new(3))
                .unwrap()
                .files[0]
                .parts[0]
                .durable,
            500,
            "a resumed file's durable progress must survive restore"
        );
        let plan = effects.iter().find_map(|e| match e {
            Effect::PrepareFile { file: f, plan } if *f == file => Some(plan.clone()),
            _ => None,
        });
        assert_eq!(plan.unwrap().resume_final_name, Some("f.bin".to_string()));
    }

    #[test]
    fn restore_demotes_downloading_to_paused_when_auto_resume_is_off() {
        let package = sample_package(FileStatus::Active);
        let config = EngineConfig {
            auto_resume_on_startup: false,
            ..EngineConfig::default()
        };
        let (state, _effects) = EngineState::restore(config, &sample_queue(), vec![package]);
        assert_eq!(
            state
                .package(crate::model::PackageId::new(3))
                .unwrap()
                .status,
            PackageStatus::Paused
        );
    }

    #[test]
    fn restore_reprobes_a_file_that_never_finished_activating() {
        let package = sample_package(FileStatus::Preparing);
        let (state, effects) =
            EngineState::restore(EngineConfig::default(), &sample_queue(), vec![package]);

        let file = state
            .package(crate::model::PackageId::new(3))
            .unwrap()
            .files[0]
            .id;
        assert_eq!(
            state
                .package(crate::model::PackageId::new(3))
                .unwrap()
                .files[0]
                .status,
            FileStatus::Unprobed
        );
        assert!(
            state
                .package(crate::model::PackageId::new(3))
                .unwrap()
                .files[0]
                .parts
                .is_empty()
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::Probe { file: f, .. } if *f == file))
        );
    }

    #[test]
    fn restore_seeds_id_counters_from_the_queue_manifest() {
        let package = sample_package(FileStatus::Verified);
        let (mut state, _effects) =
            EngineState::restore(EngineConfig::default(), &sample_queue(), vec![package]);

        let now = Instant::now();
        let effects = state.handle(
            Input::Command(Command::AddPackage {
                name: "second".to_string(),
                dest_dir: std::path::PathBuf::from("/tmp/nuxd-sim"),
                sources: vec!["http://example.com/g.bin".to_string()],
                priority: Priority::Normal,
            }),
            now,
        );
        let new_package = package_added(&effects).unwrap();
        assert_eq!(new_package, crate::model::PackageId::new(4));
    }

    fn link(id: u64, name: &str, slots: u8) -> Link {
        Link {
            id: LinkId::new(id),
            name: name.to_string(),
            label: name.to_string(),
            kind: LinkKind::Other,
            v4: Some(Ipv4Addr::new(10, 0, u8::try_from(id).unwrap_or(0), 2)),
            v6: None,
            max_slots: slots,
            limit: None,
        }
    }

    #[test]
    fn a_link_lost_mid_block_aborts_uncounted_and_moves_the_block() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(
            Input::LinksChanged(vec![link(1, "a0", 4), link(2, "b0", 4)]),
            now,
        );
        let (_, file) = add_single_file(&mut state, now, "http://example.com/f.bin");
        let (attempt, _) =
            activate_and_start(&mut state, now, file, "http://example.com/f.bin", 1000);
        let on_link = state.attempt_link(attempt);

        let survivors: Vec<Link> = state
            .links()
            .iter()
            .filter(|l| l.id != on_link)
            .cloned()
            .collect();
        let effects = state.handle(Input::LinksChanged(survivors.clone()), now);
        check_invariants(&state);

        assert!(effects.iter().any(|e| matches!(
            e,
            Effect::AbortAttempt { id, reason: AbortReason::LinkLost } if *id == attempt
        )));
        let (restarted, _) = find_start_attempt(&effects, file)
            .unwrap_or_else(|| panic!("block should restart on the surviving link: {effects:?}"));
        assert_ne!(restarted, attempt);
        assert_eq!(state.attempt_link(restarted), survivors[0].id);
        assert_eq!(state.block_retries(file, 0), 0, "LinkLost must not count");
        assert!(effects.contains(&Effect::Emit(EngineEvent::LinksChanged)));
    }

    #[test]
    fn losing_the_only_link_parks_the_block_until_a_link_returns() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![link(1, "a0", 4)]), now);
        let (_, file) = add_single_file(&mut state, now, "http://example.com/f.bin");
        activate_and_start(&mut state, now, file, "http://example.com/f.bin", 1000);

        let effects = state.handle(Input::LinksChanged(Vec::new()), now);
        assert_eq!(state.attempt_count(), 0);
        assert!(find_start_attempt(&effects, file).is_none());

        let effects = state.handle(Input::LinksChanged(vec![link(1, "a0", 4)]), now);
        assert!(find_start_attempt(&effects, file).is_some());
        assert_eq!(state.block_retries(file, 0), 0);
    }

    #[test]
    fn a_link_added_mid_download_takes_pending_blocks_immediately() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![link(1, "a0", 1)]), now);
        let (_, file) = add_single_file(&mut state, now, "http://example.com/big.bin");
        // Several blocks, but link a0 only has one slot.
        activate_and_start(
            &mut state,
            now,
            file,
            "http://example.com/big.bin",
            64 * 1024 * 1024,
        );
        assert_eq!(state.attempt_count(), 1);

        let effects = state.handle(
            Input::LinksChanged(vec![link(1, "a0", 1), link(2, "b0", 3)]),
            now,
        );
        check_invariants(&state);
        let started_on_new: Vec<_> = effects
            .iter()
            .filter(|e| matches!(e, Effect::StartAttempt { link, .. } if *link == LinkId::new(2)))
            .collect();
        assert_eq!(started_on_new.len(), 3, "new link's slots fill at once");
        assert_eq!(state.attempt_count(), 4);
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::AbortAttempt { .. })),
            "existing attempts are untouched"
        );
    }

    #[test]
    fn a_link_whose_address_changed_is_treated_as_lost_and_replaced() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![link(1, "a0", 4)]), now);
        let (_, file) = add_single_file(&mut state, now, "http://example.com/f.bin");
        let (attempt, _) =
            activate_and_start(&mut state, now, file, "http://example.com/f.bin", 1000);

        let mut moved = link(1, "a0", 4);
        moved.v4 = Some(Ipv4Addr::new(192, 168, 9, 9));
        let effects = state.handle(Input::LinksChanged(vec![moved]), now);
        assert!(effects.iter().any(|e| matches!(
            e,
            Effect::AbortAttempt { id, reason: AbortReason::LinkLost } if *id == attempt
        )));
        assert!(find_start_attempt(&effects, file).is_some());
    }

    #[test]
    fn an_unchanged_scan_does_nothing() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![link(1, "a0", 4)]), now);
        let effects = state.handle(Input::LinksChanged(vec![link(1, "a0", 4)]), now);
        assert!(effects.is_empty());
    }

    // --- link health (P3.5) ---

    fn fail_connect(
        state: &mut EngineState,
        now: Instant,
        attempt: crate::model::AttemptId,
        position: u64,
    ) -> Vec<Effect> {
        let effects = state.handle(
            Input::AttemptEnded {
                id: attempt,
                outcome: AttemptOutcome::Failed(TransferError::Connect("refused".to_string())),
                position,
            },
            now,
        );
        check_invariants(state);
        effects
    }

    #[test]
    fn a_dead_link_degrades_and_never_burns_the_blocks_retries() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![link(1, "usb0", 4)]), now);
        let (_, file) = add_single_file(&mut state, now, "http://example.com/f.bin");
        let (mut attempt, range) =
            activate_and_start(&mut state, now, file, "http://example.com/f.bin", 1000);

        for failure in 1..=2 {
            let effects = fail_connect(&mut state, now, attempt, range.start);
            (attempt, _) = find_start_attempt(&effects, file)
                .unwrap_or_else(|| panic!("failure {failure} should retry at once"));
        }
        let effects = fail_connect(&mut state, now, attempt, range.start);
        assert!(
            find_start_attempt(&effects, file).is_none(),
            "3rd failure degrades"
        );
        let until = now + Duration::from_secs(5);
        assert_eq!(
            state.link_state(LinkId::new(1)),
            LinkState::Degraded { until }
        );
        assert!(effects.contains(&Effect::WakeAt(until)));
        assert_eq!(
            state.block_retries(file, 0),
            0,
            "link blame is never counted"
        );

        // Still skipped just before the backoff ends…
        let effects = state.handle(
            Input::Tick {
                progress: Vec::new(),
            },
            now + Duration::from_secs(4),
        );
        assert!(find_start_attempt(&effects, file).is_none());
        // …and one trial afterwards.
        let effects = state.handle(
            Input::Tick {
                progress: Vec::new(),
            },
            until,
        );
        let (trial, _) = find_start_attempt(&effects, file)
            .unwrap_or_else(|| panic!("half-open trial expected: {effects:?}"));

        // The trial fails too: degraded again, twice as long.
        let effects = fail_connect(&mut state, until, trial, range.start);
        assert!(find_start_attempt(&effects, file).is_none());
        let until2 = until + Duration::from_secs(10);
        assert_eq!(
            state.link_state(LinkId::new(1)),
            LinkState::Degraded { until: until2 }
        );
        assert_eq!(state.block_retries(file, 0), 0);
    }

    #[test]
    fn a_recovering_link_returns_to_up_after_a_successful_trial() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![link(1, "usb0", 4)]), now);
        let (_, file) = add_single_file(&mut state, now, "http://example.com/f.bin");
        let (mut attempt, range) =
            activate_and_start(&mut state, now, file, "http://example.com/f.bin", 1000);
        for _ in 0..2 {
            let effects = fail_connect(&mut state, now, attempt, range.start);
            (attempt, _) = find_start_attempt(&effects, file).unwrap_or_else(|| panic!("retry"));
        }
        fail_connect(&mut state, now, attempt, range.start);
        let until = now + Duration::from_secs(5);

        let effects = state.handle(
            Input::Tick {
                progress: Vec::new(),
            },
            until,
        );
        let (trial, _) = find_start_attempt(&effects, file).unwrap_or_else(|| panic!("trial"));
        // Bytes flow: the circuit closes.
        state.handle(
            Input::Tick {
                progress: vec![(trial, range.start + 100)],
            },
            until + Duration::from_millis(250),
        );
        assert_eq!(state.link_state(LinkId::new(1)), LinkState::Up);
        assert_eq!(state.link_bytes(LinkId::new(1)), 100);
    }

    #[test]
    fn a_degraded_link_is_skipped_while_a_healthy_one_carries_on() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(
            Input::LinksChanged(vec![link(1, "dead0", 1), link(2, "good0", 1)]),
            now,
        );
        let (_, file) = add_single_file(&mut state, now, "http://example.com/big.bin");
        state.handle(
            Input::ProbeDone {
                file,
                result: ok_probe("http://example.com/big.bin", 64 * 1024 * 1024),
            },
            now,
        );
        let mut effects = state.handle(
            Input::FilePrepared {
                file,
                result: Ok(PreparedFile {
                    final_name: "big.bin".to_string(),
                    frontiers_valid: true,
                }),
            },
            now,
        );
        // Kill link 1 by failing its attempts three times over.
        for _ in 0..3 {
            let (id, on_dead, range) = effects
                .iter()
                .find_map(|e| match e {
                    Effect::StartAttempt {
                        id, link, range, ..
                    } if *link == LinkId::new(1) => Some((*id, *link, *range)),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("dead link should have an attempt: {effects:?}"));
            assert_eq!(on_dead, LinkId::new(1));
            effects = fail_connect(&mut state, now, id, range.start);
        }
        assert!(matches!(
            state.link_state(LinkId::new(1)),
            LinkState::Degraded { .. }
        ));
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::StartAttempt { link, .. } if *link == LinkId::new(1)))
        );
        // The healthy link still holds its own attempt.
        assert_eq!(
            state
                .snapshot()
                .links
                .iter()
                .map(|l| l.active)
                .collect::<Vec<_>>(),
            [0, 1]
        );
    }

    // --- multi-link scheduling and attribution (P3.6, P3.7) ---

    struct Run {
        id: crate::model::AttemptId,
        link: LinkId,
        range: crate::model::ByteRange,
        started: Duration,
        finishes: Duration,
    }

    /// Runs one 64 MiB file over three 2-slot links whose speeds
    /// (bytes/s) are `speeds`, in virtual time, checking after every
    /// event that per-link byte attribution equals the parts' frontiers.
    /// Returns how many blocks each link completed.
    fn simulate_three_links(speeds: [u64; 3]) -> [u32; 3] {
        let size = 64 * 1024 * 1024;
        let mut state = EngineState::new(EngineConfig::default());
        let base = Instant::now();
        let links: Vec<Link> = (1..=3u64)
            .map(|id| link(id, &format!("l{id}"), 2))
            .collect();
        state.handle(Input::LinksChanged(links), base);
        let (_, file) = add_single_file(&mut state, base, "http://example.com/big.bin");
        state.handle(
            Input::ProbeDone {
                file,
                result: ok_probe("http://example.com/big.bin", size),
            },
            base,
        );
        let mut effects = state.handle(
            Input::FilePrepared {
                file,
                result: Ok(PreparedFile {
                    final_name: "big.bin".to_string(),
                    frontiers_valid: true,
                }),
            },
            base,
        );

        let speed_of = |link: LinkId| speeds[usize::try_from(link.get() - 1).unwrap_or(0)];
        let mut running: Vec<Run> = Vec::new();
        let mut done = [0u32; 3];
        let mut clock = Duration::ZERO;

        let launch = |effects: &[Effect], clock: Duration, running: &mut Vec<Run>| {
            for effect in effects {
                if let Effect::StartAttempt {
                    id, link, range, ..
                } = effect
                {
                    let len = range.end.map_or(0, |end| end - range.start + 1);
                    let secs = len as f64 / speed_of(*link) as f64;
                    running.push(Run {
                        id: *id,
                        link: *link,
                        range: *range,
                        started: clock,
                        finishes: clock + Duration::from_secs_f64(secs),
                    });
                }
            }
        };
        launch(&effects, clock, &mut running);

        while !running.is_empty() {
            let next = running
                .iter()
                .enumerate()
                .min_by_key(|(_, run)| run.finishes)
                .map(|(index, _)| index)
                .unwrap_or_else(|| panic!("running is non-empty"));
            let finished = running.remove(next);
            clock = finished.finishes;
            let now = base + clock;

            // A tick with every other attempt's partial progress first.
            let progress: Vec<_> = running
                .iter()
                .map(|run| {
                    let total = (run.finishes - run.started).as_secs_f64();
                    let frac = ((clock - run.started).as_secs_f64() / total).clamp(0.0, 1.0);
                    let len = run.range.end.map_or(0, |end| end - run.range.start + 1);
                    (run.id, run.range.start + (len as f64 * frac) as u64)
                })
                .collect();
            state.handle(Input::Tick { progress }, now);
            assert_attribution(&state, file);

            let end = finished.range.end.map_or(0, |end| end + 1);
            effects = state.handle(
                Input::AttemptEnded {
                    id: finished.id,
                    outcome: AttemptOutcome::Success,
                    position: end,
                },
                now,
            );
            check_invariants(&state);
            assert_attribution(&state, file);
            done[usize::try_from(finished.link.get() - 1).unwrap_or(0)] += 1;
            launch(&effects, clock, &mut running);
        }

        let entry = state.file(file).unwrap_or_else(|| panic!("file vanished"));
        let layout = entry.layout.unwrap_or_else(|| panic!("no layout"));
        assert_eq!(
            done.iter().sum::<u32>(),
            layout.count,
            "every block ran once"
        );
        assert_eq!(state.attempt_count(), 0);
        done
    }

    /// Per-link byte attribution always equals the frontiers (§4).
    fn assert_attribution(state: &EngineState, file: crate::model::FileId) {
        let durable: u64 = state
            .file(file)
            .map_or(0, |entry| entry.parts.iter().map(|p| p.durable).sum());
        let credited: u64 = state.links().iter().map(|l| state.link_bytes(l.id)).sum();
        assert_eq!(credited, durable, "link attribution must equal frontiers");
    }

    #[test]
    fn faster_links_take_more_blocks() {
        let [fast, medium, slow] = simulate_three_links([8_000_000, 4_000_000, 1_000_000]);
        assert!(fast > medium, "fast {fast} vs medium {medium}");
        assert!(medium > slow, "medium {medium} vs slow {slow}");
        assert!(slow >= 1, "even the slow link takes some work");
    }

    #[test]
    fn equal_links_share_the_start_evenly() {
        let [a, b, c] = simulate_three_links([2_000_000, 2_000_000, 2_000_000]);
        assert!(a.abs_diff(b) <= 2 && b.abs_diff(c) <= 2, "{a} {b} {c}");
    }

    #[test]
    fn the_first_blocks_are_interleaved_across_links() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(
            Input::LinksChanged(vec![link(1, "a", 2), link(2, "b", 2), link(3, "c", 2)]),
            now,
        );
        let (_, file) = add_single_file(&mut state, now, "http://example.com/big.bin");
        state.handle(
            Input::ProbeDone {
                file,
                result: ok_probe("http://example.com/big.bin", 64 * 1024 * 1024),
            },
            now,
        );
        let effects = state.handle(
            Input::FilePrepared {
                file,
                result: Ok(PreparedFile {
                    final_name: "big.bin".to_string(),
                    frontiers_valid: true,
                }),
            },
            now,
        );
        let order: Vec<u64> = effects
            .iter()
            .filter_map(|e| match e {
                Effect::StartAttempt { link, .. } => Some(link.get()),
                _ => None,
            })
            .collect();
        assert_eq!(order, [1, 2, 3, 1, 2, 3], "round-robin, not link-at-a-time");
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(24))]
        #[test]
        fn attribution_matches_frontiers_for_any_speeds(
            a in 200_000u64..20_000_000,
            b in 200_000u64..20_000_000,
            c in 200_000u64..20_000_000,
        ) {
            // `simulate_three_links` asserts the property after every event.
            let done = simulate_three_links([a, b, c]);
            proptest::prop_assert!(done.iter().sum::<u32>() > 0);
        }
    }
}
