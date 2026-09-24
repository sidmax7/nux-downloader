//! A scripted simulator harness — the Rust counterpart of Plexo's
//! `simDownload` (`ARCHITECTURE.md` §14.2). Drives `EngineState::handle`
//! directly with hand-built `Input`s standing in for a driver's real
//! effects (fake transfers, fake probes, fake I/O), and checks the §4
//! invariants hold after every step. No sockets, no files, no tokio.

use super::{Effect, EngineEvent, EngineState};
use crate::error::ProbeError;
use crate::model::{Link, LinkId, LinkKind, PackageStatus};
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
}
