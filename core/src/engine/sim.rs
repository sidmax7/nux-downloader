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
        if matches!(
            package.status,
            PackageStatus::Done | PackageStatus::Verifying | PackageStatus::Extracting
        ) {
            assert!(
                package.all_files_verified(),
                "package {:?}: {:?} but not every file is Verified",
                package.id,
                package.status
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
        content_type: None,
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
                post: Default::default(),
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
        // Every byte is in, but the package is Done only once verified.
        assert_eq!(pkg.status, PackageStatus::Verifying);
        assert!(!effects.contains(&Effect::Emit(EngineEvent::PackageDone { package })));
        let effects = state.handle(
            Input::VerifyDone {
                package,
                result: Ok(Default::default()),
            },
            now,
        );
        check_invariants(&state);
        assert!(effects.contains(&Effect::Emit(EngineEvent::PackageDone { package })));
        assert_eq!(state.package(package).unwrap().status, PackageStatus::Done);
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
        assert_eq!(pkg.status, PackageStatus::Verifying);
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::VerifyPackage { package: p, .. } if *p == package))
        );
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
                post: Default::default(),
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
    fn activation_waits_while_finished_files_wait_for_their_rename() {
        let config = EngineConfig {
            max_pending_finalizes: 1,
            ..EngineConfig::default()
        };
        let mut state = EngineState::new(config);
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![link(1, "eth0", 1)]), now);

        let urls = ["http://a.example.com/f.bin", "http://b.example.com/f.bin"];
        let effects = state.handle(
            Input::Command(Command::AddPackage {
                name: "pkg".to_string(),
                dest_dir: std::path::PathBuf::from("/tmp/nuxd-sim"),
                sources: urls.iter().map(ToString::to_string).collect(),
                priority: Priority::Normal,
                post: Default::default(),
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
        let (attempt, _) = activate_and_start(&mut state, now, files[0], urls[0], 500);
        // Probed while the only slot is busy, so it waits as `Queued`.
        let effects = state.handle(
            Input::ProbeDone {
                file: files[1],
                result: ok_probe(urls[1], 500),
            },
            now,
        );
        assert!(!find_prepare(&effects, files[1]));

        // The first file finishes and frees the only slot, but its rename
        // is still queued: the second file must not be activated yet.
        let effects = state.handle(
            Input::AttemptEnded {
                id: attempt,
                outcome: AttemptOutcome::Success,
                position: 500,
            },
            now,
        );
        check_invariants(&state);
        assert!(effects.contains(&Effect::Finalize { file: files[0] }));
        assert!(!find_prepare(&effects, files[1]), "{effects:?}");
        let effects = state.handle(Input::Tick { progress: vec![] }, now);
        assert!(!find_prepare(&effects, files[1]));

        // Once it's renamed, the next file goes.
        let effects = state.handle(Input::FinalizeDone { file: files[0] }, now);
        check_invariants(&state);
        assert!(find_prepare(&effects, files[1]), "{effects:?}");
    }

    /// Decision 34: the displayed speed is measured on the engine's own
    /// tick clock, so a steady download reads steady, and a paused one
    /// reads 0 at once.
    #[test]
    fn the_displayed_speed_is_measured_on_the_engine_clock() {
        let mut state = EngineState::new(EngineConfig::default());
        let t0 = Instant::now();
        state.handle(Input::LinksChanged(vec![one_link()]), t0);
        let url = "http://example.com/f.bin";
        let (package, file) = add_single_file(&mut state, t0, url);
        let (attempt, _) = activate_and_start(&mut state, t0, file, url, 4_000_000);

        // 1 MB/s, sampled every 250 ms.
        for i in 1..=16u64 {
            let at = t0 + Duration::from_millis(250 * i);
            state.handle(
                Input::Tick {
                    progress: vec![(attempt, i * 250_000)],
                },
                at,
            );
            if i >= 2 {
                let snapshot = state.snapshot();
                let speed = snapshot.file_speeds[&file];
                assert!((speed - 1_000_000.0).abs() < 1.0, "tick {i}: {speed}");
                assert!((snapshot.links[0].speed - 1_000_000.0).abs() < 1.0);
            }
        }

        let after = t0 + Duration::from_millis(250 * 17);
        state.handle(Input::Command(Command::Pause(package)), after);
        state.handle(Input::Tick { progress: vec![] }, after);
        assert!(!state.snapshot().file_speeds.contains_key(&file));
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
                post: Default::default(),
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
        let (mut state, effects) =
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
        // The probe pool issues the probe on the engine's first step.
        assert!(!effects.iter().any(|e| matches!(e, Effect::Probe { .. })));
        let effects = state.handle(
            Input::Tick {
                progress: Vec::new(),
            },
            Instant::now(),
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
                post: Default::default(),
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
        // This models plain delivery; tail handling has its own tests.
        let mut state = EngineState::new(EngineConfig {
            max_hedges_per_block: 0,
            max_refreshes_per_block: 0,
            ..EngineConfig::default()
        });
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

    // --- host backoff (§16: 429/503 → host backoff, no retry counted) ---

    fn refuse(
        state: &mut EngineState,
        now: Instant,
        attempt: crate::model::AttemptId,
        position: u64,
    ) -> Vec<Effect> {
        let effects = state.handle(
            Input::AttemptEnded {
                id: attempt,
                outcome: AttemptOutcome::Failed(TransferError::Status(429)),
                position,
            },
            now,
        );
        check_invariants(state);
        effects
    }

    #[test]
    fn a_429_backs_the_host_off_without_charging_the_block() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![link(1, "a0", 4)]), now);
        let (_, file) = add_single_file(&mut state, now, "http://example.com/f.bin");
        let (attempt, range) =
            activate_and_start(&mut state, now, file, "http://example.com/f.bin", 1000);

        let effects = refuse(&mut state, now, attempt, range.start);
        assert!(
            find_start_attempt(&effects, file).is_none(),
            "host is cooling down"
        );
        assert_eq!(state.block_retries(file, 0), 0);
        assert!(effects.contains(&Effect::WakeAt(now + Duration::from_secs(2))));

        let early = state.handle(
            Input::Tick {
                progress: Vec::new(),
            },
            now + Duration::from_secs(1),
        );
        assert!(find_start_attempt(&early, file).is_none());
        let later = state.handle(
            Input::Tick {
                progress: Vec::new(),
            },
            now + Duration::from_secs(2),
        );
        assert!(
            find_start_attempt(&later, file).is_some(),
            "retries after the backoff"
        );
    }

    #[test]
    fn repeated_refusals_lengthen_the_backoff_and_eventually_fail_the_file() {
        let mut state = EngineState::new(EngineConfig::default());
        let mut now = Instant::now();
        state.handle(Input::LinksChanged(vec![link(1, "a0", 4)]), now);
        let (_, file) = add_single_file(&mut state, now, "http://example.com/f.bin");
        let (mut attempt, range) =
            activate_and_start(&mut state, now, file, "http://example.com/f.bin", 1000);

        let mut waits = Vec::new();
        for _ in 0..12 {
            let effects = refuse(&mut state, now, attempt, range.start);
            if state
                .file(file)
                .is_some_and(|f| matches!(f.status, FileStatus::Failed(_)))
            {
                break;
            }
            let wake = effects
                .iter()
                .find_map(|e| match e {
                    Effect::WakeAt(t) => Some(*t),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("no WakeAt in {effects:?}"));
            waits.push(wake - now);
            now = wake;
            let effects = state.handle(
                Input::Tick {
                    progress: Vec::new(),
                },
                now,
            );
            attempt = find_start_attempt(&effects, file)
                .unwrap_or_else(|| panic!("should retry after backoff"))
                .0;
        }
        assert_eq!(waits[0], Duration::from_secs(2));
        assert_eq!(waits[1], Duration::from_secs(4));
        assert_eq!(*waits.iter().max().unwrap(), Duration::from_secs(60));
        assert!(
            matches!(
                state.file(file).map(|f| f.status.clone()),
                Some(FileStatus::Failed(_))
            ),
            "a host that never relents fails the file"
        );
    }

    #[test]
    fn a_refused_host_learns_a_lower_connection_cap() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![link(1, "a0", 8)]), now);
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
        let started: Vec<_> = effects
            .iter()
            .filter_map(|e| match e {
                Effect::StartAttempt { id, range, .. } => Some((*id, *range)),
                _ => None,
            })
            .collect();
        assert_eq!(started.len(), 8);

        // The host refuses the ninth… here, one of the eight.
        refuse(&mut state, now, started[0].0, started[0].1.start);
        let after = now + Duration::from_secs(2);
        let effects = state.handle(
            Input::Tick {
                progress: Vec::new(),
            },
            after,
        );
        // 7 still running; the host is capped at 7/2 = 3, so no new starts.
        assert!(find_start_attempt(&effects, file).is_none());
        assert_eq!(state.attempt_count(), 7);
    }

    #[test]
    fn a_429_on_the_probe_is_retried_after_the_backoff() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![link(1, "a0", 4)]), now);
        let (_, file) = add_single_file(&mut state, now, "http://example.com/f.bin");

        let effects = state.handle(
            Input::ProbeDone {
                file,
                result: Err(crate::error::ProbeError::UnexpectedStatus { status: 429 }),
            },
            now,
        );
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::Emit(EngineEvent::FileFailed { .. })))
        );
        assert!(effects.contains(&Effect::WakeAt(now + Duration::from_secs(2))));

        let early = state.handle(
            Input::Tick {
                progress: Vec::new(),
            },
            now + Duration::from_secs(1),
        );
        assert!(!early.iter().any(|e| matches!(e, Effect::Probe { .. })));
        let due = state.handle(
            Input::Tick {
                progress: Vec::new(),
            },
            now + Duration::from_secs(2),
        );
        assert!(
            due.iter()
                .any(|e| matches!(e, Effect::Probe { file: f, .. } if *f == file))
        );

        // A different failure is still fatal.
        let effects = state.handle(
            Input::ProbeDone {
                file,
                result: Err(crate::error::ProbeError::UnexpectedStatus { status: 404 }),
            },
            now + Duration::from_secs(3),
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::Emit(EngineEvent::FileFailed { .. })))
        );
    }

    #[test]
    fn a_probe_that_keeps_getting_429_eventually_fails() {
        let mut state = EngineState::new(EngineConfig::default());
        let mut now = Instant::now();
        state.handle(Input::LinksChanged(vec![link(1, "a0", 4)]), now);
        let (_, file) = add_single_file(&mut state, now, "http://example.com/f.bin");
        let mut failed = false;
        for _ in 0..10 {
            let effects = state.handle(
                Input::ProbeDone {
                    file,
                    result: Err(crate::error::ProbeError::UnexpectedStatus { status: 503 }),
                },
                now,
            );
            if effects
                .iter()
                .any(|e| matches!(e, Effect::Emit(EngineEvent::FileFailed { .. })))
            {
                failed = true;
                break;
            }
            now += Duration::from_secs(3600);
            let due = state.handle(
                Input::Tick {
                    progress: Vec::new(),
                },
                now,
            );
            assert!(due.iter().any(|e| matches!(e, Effect::Probe { .. })));
        }
        assert!(failed, "probe retries are bounded");
    }

    #[test]
    fn a_burst_of_simultaneous_refusals_is_one_strike() {
        // Found against a real host: 7 of 8 parallel requests were turned
        // away in the same instant, and counting each as a strike sent
        // the backoff straight to 60 s.
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![link(1, "a0", 8)]), now);
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
        let started: Vec<_> = effects
            .iter()
            .filter_map(|e| match e {
                Effect::StartAttempt { id, range, .. } => Some((*id, range.start)),
                _ => None,
            })
            .collect();
        assert_eq!(started.len(), 8);

        // All but the first are refused at once.
        let mut wakes = Vec::new();
        for (id, start) in &started[1..] {
            for effect in refuse(&mut state, now, *id, *start) {
                if let Effect::WakeAt(t) = effect {
                    wakes.push(t - now);
                }
            }
        }
        assert_eq!(wakes, [Duration::from_secs(2)], "one backoff, not seven");

        // After it, one connection at a time (only one was accepted).
        let effects = state.handle(
            Input::Tick {
                progress: Vec::new(),
            },
            now + Duration::from_secs(2),
        );
        assert!(
            find_start_attempt(&effects, file).is_none(),
            "cap is 1 and the accepted attempt is still running"
        );
        assert_eq!(state.attempt_count(), 1);
    }

    #[test]
    fn a_host_refusing_one_link_does_not_slow_another() {
        // Hosts limit per client IP and each link is its own IP (§7.1).
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(
            Input::LinksChanged(vec![link(1, "a0", 2), link(2, "b0", 2)]),
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
        let on = |effects: &[Effect], link: u64| -> Vec<(crate::model::AttemptId, u64)> {
            effects
                .iter()
                .filter_map(|e| match e {
                    Effect::StartAttempt {
                        id, link: l, range, ..
                    } if l.get() == link => Some((*id, range.start)),
                    _ => None,
                })
                .collect()
        };
        let a = on(&effects, 1);
        let b = on(&effects, 2);
        assert_eq!((a.len(), b.len()), (2, 2));

        // Link 1 is refused; link 2 finishes a block normally.
        refuse(&mut state, now, a[0].0, a[0].1);
        let effects = state.handle(
            Input::AttemptEnded {
                id: b[0].0,
                outcome: AttemptOutcome::Success,
                position: b[0].1 + 2 * 1024 * 1024,
            },
            now,
        );
        assert_eq!(on(&effects, 2).len(), 1, "link 2 refills its slot at once");
        assert!(on(&effects, 1).is_empty(), "link 1 is cooling down");
    }

    #[test]
    fn retry_after_sets_a_floor_under_the_backoff() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![link(1, "a0", 4)]), now);
        let (_, file) = add_single_file(&mut state, now, "http://example.com/f.bin");
        let (attempt, range) =
            activate_and_start(&mut state, now, file, "http://example.com/f.bin", 1000);
        let effects = state.handle(
            Input::AttemptEnded {
                id: attempt,
                outcome: AttemptOutcome::Failed(TransferError::Busy {
                    status: 429,
                    retry_after: Some(Duration::from_secs(10)),
                }),
                position: range.start,
            },
            now,
        );
        assert!(effects.contains(&Effect::WakeAt(now + Duration::from_secs(10))));
        assert_eq!(state.block_retries(file, 0), 0);
        let at_9 = state.handle(
            Input::Tick {
                progress: Vec::new(),
            },
            now + Duration::from_secs(9),
        );
        assert!(find_start_attempt(&at_9, file).is_none());
        let at_10 = state.handle(
            Input::Tick {
                progress: Vec::new(),
            },
            now + Duration::from_secs(10),
        );
        assert!(find_start_attempt(&at_10, file).is_some());
    }

    // --- queue order and priority (P4.1) ---

    /// Adds a one-file package and activates its file, returning ids and
    /// the effects the activation produced.
    fn add_and_activate(
        state: &mut EngineState,
        now: Instant,
        url: &str,
    ) -> (crate::model::PackageId, crate::model::FileId, Vec<Effect>) {
        let (package, file) = add_single_file(state, now, url);
        state.handle(
            Input::ProbeDone {
                file,
                result: ok_probe(url, 1000),
            },
            now,
        );
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
        (package, file, effects)
    }

    fn finish(
        state: &mut EngineState,
        now: Instant,
        attempt: crate::model::AttemptId,
        end: u64,
    ) -> Vec<Effect> {
        let effects = state.handle(
            Input::AttemptEnded {
                id: attempt,
                outcome: AttemptOutcome::Success,
                position: end,
            },
            now,
        );
        check_invariants(state);
        effects
    }

    /// One slot, three one-block files A (running), B, C (waiting).
    fn abc() -> (
        EngineState,
        Instant,
        [crate::model::PackageId; 3],
        [crate::model::FileId; 3],
        crate::model::AttemptId,
    ) {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![link(1, "a0", 1)]), now);
        let (pa, fa, effects) = add_and_activate(&mut state, now, "http://a.example.com/a.bin");
        let (attempt, _) = find_start_attempt(&effects, fa)
            .unwrap_or_else(|| panic!("A should start: {effects:?}"));
        let (pb, fb, _) = add_and_activate(&mut state, now, "http://b.example.com/b.bin");
        let (pc, fc, _) = add_and_activate(&mut state, now, "http://c.example.com/c.bin");
        assert_eq!(state.attempt_count(), 1, "only A holds the single slot");
        (state, now, [pa, pb, pc], [fa, fb, fc], attempt)
    }

    #[test]
    fn work_follows_queue_order_by_default() {
        let (mut state, now, _, [_, fb, _], a) = abc();
        let effects = finish(&mut state, now, a, 1000);
        assert!(
            find_start_attempt(&effects, fb).is_some(),
            "B is next in line"
        );
    }

    #[test]
    fn moving_a_package_up_mid_download_changes_who_gets_the_next_slot() {
        let (mut state, now, [_, _, pc], [_, fb, fc], a) = abc();
        let effects = state.handle(
            Input::Command(Command::MoveTo {
                package: pc,
                position: 0,
            }),
            now,
        );
        check_invariants(&state);
        assert!(effects.contains(&Effect::Emit(EngineEvent::QueueReordered)));
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::AbortAttempt { .. })),
            "the running attempt is not interrupted"
        );
        assert_eq!(state.packages()[0].id, pc);

        let effects = finish(&mut state, now, a, 1000);
        assert!(
            find_start_attempt(&effects, fc).is_some(),
            "C jumped the queue"
        );
        assert!(find_start_attempt(&effects, fb).is_none());
    }

    #[test]
    fn moving_a_package_down_and_clamping_past_the_end() {
        let (mut state, now, [pa, pb, pc], _, _) = abc();
        state.handle(
            Input::Command(Command::MoveTo {
                package: pa,
                position: 99,
            }),
            now,
        );
        let order: Vec<_> = state.packages().iter().map(|p| p.id).collect();
        assert_eq!(order, [pb, pc, pa]);
        // Lookups still work after the shuffle.
        assert_eq!(state.package(pa).map(|p| p.id), Some(pa));
        assert_eq!(state.package(pc).map(|p| p.id), Some(pc));
    }

    #[test]
    fn a_higher_priority_outranks_queue_position() {
        let (mut state, now, [_, _, pc], [_, fb, fc], a) = abc();
        let effects = state.handle(
            Input::Command(Command::SetPriority {
                package: pc,
                priority: Priority::High,
            }),
            now,
        );
        check_invariants(&state);
        // The change is checkpointed before it is announced (§9.3).
        let checkpoint = effects
            .iter()
            .position(|e| matches!(e, Effect::Checkpoint { package, .. } if *package == pc));
        let event = effects
            .iter()
            .position(|e| matches!(e, Effect::Emit(EngineEvent::PackagePriorityChanged { .. })));
        assert!(checkpoint.is_some() && event.is_some() && checkpoint < event);
        // C keeps its queue position…
        assert_eq!(state.packages()[2].id, pc);
        // …but wins the next slot.
        let effects = finish(&mut state, now, a, 1000);
        assert!(find_start_attempt(&effects, fc).is_some());
        assert!(find_start_attempt(&effects, fb).is_none());
    }

    #[test]
    fn lowering_a_priority_lets_later_packages_go_first() {
        let (mut state, now, [_, pb, _], [_, fb, fc], a) = abc();
        state.handle(
            Input::Command(Command::SetPriority {
                package: pb,
                priority: Priority::Low,
            }),
            now,
        );
        let effects = finish(&mut state, now, a, 1000);
        assert!(find_start_attempt(&effects, fc).is_some(), "C now beats B");
        assert!(find_start_attempt(&effects, fb).is_none());
    }

    #[test]
    fn unknown_packages_and_no_op_changes_do_nothing() {
        let (mut state, now, [pa, ..], _, _) = abc();
        let ghost = crate::model::PackageId::new(999);
        for command in [
            Command::MoveTo {
                package: ghost,
                position: 0,
            },
            Command::SetPriority {
                package: ghost,
                priority: Priority::High,
            },
            Command::SetPriority {
                package: pa,
                priority: Priority::Normal, // already Normal
            },
            Command::MoveTo {
                package: pa,
                position: 0, // already first
            },
        ] {
            assert!(state.handle(Input::Command(command), now).is_empty());
        }
    }

    // --- lazy activation and caps (P4.2) ---

    /// Adds one package with a file per URL and probes them all with the
    /// given size. Returns the file ids and every effect produced.
    fn add_probed_package(
        state: &mut EngineState,
        now: Instant,
        urls: &[String],
        size: u64,
    ) -> (
        crate::model::PackageId,
        Vec<crate::model::FileId>,
        Vec<Effect>,
    ) {
        let mut effects = state.handle(
            Input::Command(Command::AddPackage {
                name: "many".to_string(),
                dest_dir: std::path::PathBuf::from("/tmp/nuxd-sim"),
                sources: urls.to_vec(),
                priority: Priority::Normal,
                post: Default::default(),
            }),
            now,
        );
        let package = package_added(&effects).unwrap_or_else(|| panic!("no PackageAdded"));
        let files: Vec<_> = state
            .package(package)
            .unwrap_or_else(|| panic!("no package"))
            .files
            .iter()
            .map(|f| f.id)
            .collect();
        for (file, url) in files.iter().zip(urls) {
            effects.extend(state.handle(
                Input::ProbeDone {
                    file: *file,
                    result: ok_probe(url, size),
                },
                now,
            ));
        }
        check_invariants(state);
        (package, files, effects)
    }

    fn prepared(final_name: &str) -> Result<PreparedFile, crate::error::StorageError> {
        Ok(PreparedFile {
            final_name: final_name.to_string(),
            frontiers_valid: true,
        })
    }

    fn prepare_targets(effects: &[Effect]) -> Vec<crate::model::FileId> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::PrepareFile { file, .. } => Some(*file),
                _ => None,
            })
            .collect()
    }

    fn started(effects: &[Effect]) -> Vec<(crate::model::FileId, LinkId)> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::StartAttempt { file, link, .. } => Some((*file, *link)),
                _ => None,
            })
            .collect()
    }

    fn status_count(state: &EngineState, status: FileStatus) -> usize {
        state
            .packages()
            .iter()
            .flat_map(|p| p.files.iter())
            .filter(|f| f.status == status)
            .count()
    }

    #[test]
    fn a_probed_file_stays_queued_until_the_scheduler_needs_it() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        // No link yet: probing must not create placeholders.
        let urls = vec!["http://h.example.com/a.bin".to_string()];
        let (_, files, effects) = add_probed_package(&mut state, now, &urls, 1000);
        assert!(prepare_targets(&effects).is_empty());
        assert_eq!(
            state.file(files[0]).map(|f| f.status.clone()),
            Some(FileStatus::Queued)
        );

        // A link appears: now the file is activated.
        let effects = state.handle(Input::LinksChanged(vec![link(1, "a0", 4)]), now);
        assert_eq!(prepare_targets(&effects), files);
        assert_eq!(
            state.file(files[0]).map(|f| f.status.clone()),
            Some(FileStatus::Preparing)
        );
    }

    #[test]
    fn many_small_files_are_activated_only_as_fast_as_slots_free_up() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![link(1, "a0", 4)]), now);
        let urls: Vec<String> = (0..20)
            .map(|i| format!("http://h{i}.example.com/f{i}.bin"))
            .collect();
        let (_, files, effects) = add_probed_package(&mut state, now, &urls, 1000);

        // 4 slots → 4 files preparing; the other 16 wait, untouched.
        assert_eq!(prepare_targets(&effects).len(), 4);
        assert_eq!(status_count(&state, FileStatus::Preparing), 4);
        assert_eq!(status_count(&state, FileStatus::Queued), 16);

        // As each is prepared it takes exactly one slot.
        let mut all_started = Vec::new();
        for file in prepare_targets(&effects) {
            let effects = state.handle(
                Input::FilePrepared {
                    file,
                    result: prepared("f.bin"),
                },
                now,
            );
            all_started.extend(started(&effects));
        }
        check_invariants(&state);
        assert_eq!(all_started.len(), 4);
        let distinct: std::collections::HashSet<_> = all_started.iter().map(|(f, _)| *f).collect();
        assert_eq!(distinct.len(), 4, "one slot per file");
        assert_eq!(state.attempt_count(), 4);
        assert_eq!(files.len(), 20);
    }

    #[test]
    fn a_finished_file_frees_a_slot_that_activates_the_next() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![link(1, "a0", 1)]), now);
        let urls: Vec<String> = (0..3)
            .map(|i| format!("http://h{i}.example.com/f{i}.bin"))
            .collect();
        let (_, files, effects) = add_probed_package(&mut state, now, &urls, 1000);
        assert_eq!(prepare_targets(&effects), [files[0]]);

        let effects = state.handle(
            Input::FilePrepared {
                file: files[0],
                result: prepared("f0.bin"),
            },
            now,
        );
        let (attempt, _) = find_start_attempt(&effects, files[0])
            .unwrap_or_else(|| panic!("f0 should start: {effects:?}"));
        let effects = finish(&mut state, now, attempt, 1000);
        // The slot is free again → the next queued file is activated.
        assert_eq!(prepare_targets(&effects), [files[1]]);
    }

    #[test]
    fn a_single_big_file_gets_every_slot_on_every_link() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(
            Input::LinksChanged(vec![link(1, "a0", 8), link(2, "b0", 8)]),
            now,
        );
        // 400 MiB → 50 blocks of 8 MiB, plenty for 16 slots.
        let urls = vec!["http://h.example.com/big.bin".to_string()];
        let (_, files, effects) = add_probed_package(&mut state, now, &urls, 400 * 1024 * 1024);
        assert_eq!(prepare_targets(&effects), files);
        let effects = state.handle(
            Input::FilePrepared {
                file: files[0],
                result: prepared("big.bin"),
            },
            now,
        );
        let started = started(&effects);
        assert_eq!(started.len(), 16, "past the soft cap: focus mode");
        assert_eq!(started.iter().filter(|(_, l)| l.get() == 1).count(), 8);
        assert_eq!(started.iter().filter(|(_, l)| l.get() == 2).count(), 8);
    }

    #[test]
    fn a_mix_gives_the_big_file_its_soft_cap_and_the_rest_go_to_other_files() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        // Two links: a host allows 8 connections per link (§7.1), so one
        // link alone could never give the big file more than 8.
        state.handle(
            Input::LinksChanged(vec![link(1, "a0", 8), link(2, "b0", 8)]),
            now,
        );
        // The big file is first in the queue, then two small ones.
        let (_, big, effects) = add_probed_package(
            &mut state,
            now,
            &["http://big.example.com/big.bin".to_string()],
            400 * 1024 * 1024,
        );
        let (_, small, _) = add_probed_package(
            &mut state,
            now,
            &[
                "http://s1.example.com/a.bin".to_string(),
                "http://s2.example.com/b.bin".to_string(),
            ],
            1000,
        );
        // The big file was activated first (it was alone), then two more
        // preparing for the leftover slots.
        assert_eq!(prepare_targets(&effects), big);
        let mut all = Vec::new();
        for file in std::iter::once(big[0]).chain(small.iter().copied()) {
            all.extend(started(&state.handle(
                Input::FilePrepared {
                    file,
                    result: prepared("x.bin"),
                },
                now,
            )));
        }
        check_invariants(&state);
        let for_big = all.iter().filter(|(f, _)| *f == big[0]).count();
        let for_small = all.iter().filter(|(f, _)| small.contains(f)).count();
        assert_eq!(for_small, 2, "each small file takes a slot");
        assert_eq!(state.attempt_count(), 16, "no slot idles while work exists");
        assert_eq!(for_big, 14, "the big file fills the rest");
    }

    #[test]
    fn a_host_active_file_limit_holds_back_activation() {
        let mut config = EngineConfig::default();
        config.hosts.add_rule(
            "*.limited.example.com",
            crate::config::HostLimits {
                max_active_files: Some(1),
                ..crate::config::HostLimits::default()
            },
        );
        let mut state = EngineState::new(config);
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![link(1, "a0", 8)]), now);
        let urls = vec![
            "http://a.limited.example.com/1.bin".to_string(),
            "http://a.limited.example.com/2.bin".to_string(),
            "http://other.example.com/3.bin".to_string(),
        ];
        let (_, files, effects) = add_probed_package(&mut state, now, &urls, 1000);
        // File 1 and the unrelated host's file activate; file 2 waits.
        let mut targets = prepare_targets(&effects);
        targets.sort();
        assert_eq!(targets, [files[0], files[2]]);
        assert_eq!(
            state.file(files[1]).map(|f| f.status.clone()),
            Some(FileStatus::Queued)
        );
    }

    #[test]
    fn a_paused_package_is_not_activated() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        let urls = vec!["http://h.example.com/a.bin".to_string()];
        let (package, files, _) = add_probed_package(&mut state, now, &urls, 1000);
        state.handle(Input::Command(Command::Pause(package)), now);
        let effects = state.handle(Input::LinksChanged(vec![link(1, "a0", 4)]), now);
        assert!(prepare_targets(&effects).is_empty());
        assert_eq!(
            state.file(files[0]).map(|f| f.status.clone()),
            Some(FileStatus::Queued)
        );
        let effects = state.handle(Input::Command(Command::Resume(package)), now);
        assert_eq!(prepare_targets(&effects), files);
    }

    // --- probe pool (P4.4) ---

    fn probes(effects: &[Effect]) -> Vec<crate::model::FileId> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Probe { file, .. } => Some(*file),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn pasting_many_links_never_exceeds_the_probe_caps() {
        let config = EngineConfig {
            max_concurrent_probes: 5,
            max_probes_per_host: 5,
            ..EngineConfig::default()
        };
        let mut state = EngineState::new(config);
        let now = Instant::now();
        let sources: Vec<String> = (0..500)
            .map(|i| format!("http://host{}.example.com/f{i}.bin", i % 50))
            .collect();
        let effects = state.handle(
            Input::Command(Command::AddPackage {
                name: "many".to_string(),
                dest_dir: std::path::PathBuf::from("/tmp/nuxd-sim"),
                sources,
                priority: Priority::Normal,
                post: Default::default(),
            }),
            now,
        );
        let mut in_flight = probes(&effects);
        assert_eq!(in_flight.len(), 5, "only the pool's worth start at once");

        // Each finished probe releases exactly one more, never more than 5.
        let mut done = 0;
        while let Some(file) = in_flight.pop() {
            let effects = state.handle(
                Input::ProbeDone {
                    file,
                    result: ok_probe("http://x.example.com/f", 1000),
                },
                now,
            );
            done += 1;
            in_flight.extend(probes(&effects));
            assert!(in_flight.len() <= 5, "pool exceeded: {}", in_flight.len());
        }
        assert_eq!(done, 500, "every file gets probed exactly once");
    }

    #[test]
    fn probes_to_one_host_are_capped_separately() {
        let config = EngineConfig {
            max_concurrent_probes: 8,
            max_probes_per_host: 2,
            ..EngineConfig::default()
        };
        let mut state = EngineState::new(config);
        let now = Instant::now();
        // Ten files on one host, then one on another.
        let mut sources: Vec<String> = (0..10)
            .map(|i| format!("http://same.example.com/f{i}.bin"))
            .collect();
        sources.push("http://other.example.com/o.bin".to_string());
        let effects = state.handle(
            Input::Command(Command::AddPackage {
                name: "p".to_string(),
                dest_dir: std::path::PathBuf::from("/tmp/nuxd-sim"),
                sources,
                priority: Priority::Normal,
                post: Default::default(),
            }),
            now,
        );
        // 2 for the crowded host, and the other host isn't starved.
        assert_eq!(probes(&effects).len(), 3);
    }

    #[test]
    fn a_paused_package_is_not_probed() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        let (package, _) = add_single_file(&mut state, now, "http://a.example.com/a.bin");
        // The add already issued its probe; pause and add another file's
        // package — nothing new is probed while paused.
        state.handle(Input::Command(Command::Pause(package)), now);
        let effects = state.handle(
            Input::Tick {
                progress: Vec::new(),
            },
            now,
        );
        assert!(probes(&effects).is_empty());
    }

    #[test]
    fn add_probed_skips_probing_and_queues_the_files() {
        let mut state = EngineState::new(EngineConfig::default());
        state.handle(Input::LinksChanged(vec![link(1, "a0", 1)]), Instant::now());
        let now = Instant::now();
        let good = crate::engine::ProbedInput {
            source: "http://a.example.com/a.bin".to_string(),
            probe: ok_probe("http://a.example.com/a.bin", 1000).map_err(|e| e.to_string()),
        };
        let bad = crate::engine::ProbedInput {
            source: "http://a.example.com/gone.bin".to_string(),
            probe: Err("404".to_string()),
        };
        let effects = state.handle(
            Input::Command(Command::AddProbed {
                name: "drafted".to_string(),
                dest_dir: std::path::PathBuf::from("/tmp/nuxd-sim"),
                files: vec![good, bad],
                priority: Priority::Normal,
                post: Default::default(),
            }),
            now,
        );
        assert!(probes(&effects).is_empty(), "already probed");
        let package = package_added(&effects).unwrap();
        let files = &state.package(package).unwrap().files;
        assert_eq!(files.len(), 2);
        // The good one goes straight to activation (a slot is free).
        assert_eq!(files[0].status, FileStatus::Preparing);
        assert!(matches!(files[1].status, FileStatus::Failed(_)));
        assert!(!prepare_targets(&effects).is_empty());
        check_invariants(&state);
    }

    // --- re-resolve on 403/410 (P4.5) ---

    fn status_failure(
        state: &mut EngineState,
        now: Instant,
        attempt: crate::model::AttemptId,
        position: u64,
        code: u16,
    ) -> Vec<Effect> {
        let effects = state.handle(
            Input::AttemptEnded {
                id: attempt,
                outcome: AttemptOutcome::Failed(TransferError::Status(code)),
                position,
            },
            now,
        );
        check_invariants(state);
        effects
    }

    #[test]
    fn an_expired_link_is_re_resolved_once_and_the_download_continues() {
        for code in [403, 410] {
            let mut state = EngineState::new(EngineConfig::default());
            let now = Instant::now();
            state.handle(Input::LinksChanged(vec![link(1, "a0", 4)]), now);
            let (_, file) = add_single_file(&mut state, now, "http://old.example.com/f.bin");
            let (attempt, range) =
                activate_and_start(&mut state, now, file, "http://old.example.com/f.bin", 1000);

            let effects = status_failure(&mut state, now, attempt, range.start, code);
            assert!(
                effects.iter().any(|e| matches!(
                    e,
                    Effect::Reresolve { file: f, source } if *f == file && source == "http://old.example.com/f.bin"
                )),
                "{code} should trigger a re-resolve: {effects:?}"
            );
            assert!(
                find_start_attempt(&effects, file).is_none(),
                "waits for the answer"
            );
            assert_eq!(state.block_retries(file, 0), 0);
            assert!(!matches!(
                state.file(file).map(|f| f.status.clone()),
                Some(FileStatus::Failed(_))
            ));

            // A fresh URL comes back: the block restarts against it.
            let fresh = ok_probe("http://new.example.com/f.bin?sig=2", 1000).unwrap();
            let effects = state.handle(
                Input::Reresolved {
                    file,
                    result: Ok(fresh),
                },
                now,
            );
            let started = effects.iter().find_map(|e| match e {
                Effect::StartAttempt { url, .. } => Some(url.to_string()),
                _ => None,
            });
            assert_eq!(
                started.as_deref(),
                Some("http://new.example.com/f.bin?sig=2")
            );
        }
    }

    #[test]
    fn a_second_expiry_fails_the_file() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![link(1, "a0", 4)]), now);
        let (_, file) = add_single_file(&mut state, now, "http://old.example.com/f.bin");
        let (attempt, range) =
            activate_and_start(&mut state, now, file, "http://old.example.com/f.bin", 1000);
        status_failure(&mut state, now, attempt, range.start, 403);
        let effects = state.handle(
            Input::Reresolved {
                file,
                result: ok_probe("http://old.example.com/f.bin", 1000).map_err(|e| e.to_string()),
            },
            now,
        );
        let (again, _) = find_start_attempt(&effects, file).unwrap();
        let effects = status_failure(&mut state, now, again, range.start, 403);
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::Reresolve { .. })),
            "only one re-resolve per file"
        );
        assert!(matches!(
            state.file(file).map(|f| f.status.clone()),
            Some(FileStatus::Failed(_))
        ));
    }

    #[test]
    fn a_failed_re_resolve_or_a_changed_size_fails_the_file() {
        for result in [
            Err("gone".to_string()),
            ok_probe("http://old.example.com/f.bin", 999_999).map_err(|e| e.to_string()),
        ] {
            let mut state = EngineState::new(EngineConfig::default());
            let now = Instant::now();
            state.handle(Input::LinksChanged(vec![link(1, "a0", 4)]), now);
            let (_, file) = add_single_file(&mut state, now, "http://old.example.com/f.bin");
            let (attempt, range) =
                activate_and_start(&mut state, now, file, "http://old.example.com/f.bin", 1000);
            status_failure(&mut state, now, attempt, range.start, 410);
            state.handle(Input::Reresolved { file, result }, now);
            assert!(matches!(
                state.file(file).map(|f| f.status.clone()),
                Some(FileStatus::Failed(_))
            ));
        }
    }

    #[test]
    fn a_non_http_scheme_becomes_a_failed_file_not_a_probe() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        let effects = state.handle(
            Input::Command(Command::AddPackage {
                name: "p".to_string(),
                dest_dir: std::path::PathBuf::from("/tmp/nuxd-sim"),
                sources: vec!["ftp://example.com/a.bin".to_string()],
                priority: Priority::Normal,
                post: Default::default(),
            }),
            now,
        );
        assert!(probes(&effects).is_empty());
        let package = package_added(&effects).unwrap();
        let status = state.package(package).unwrap().files[0].status.clone();
        assert!(
            matches!(&status, FileStatus::Failed(e) if e.to_string().contains("ftp")),
            "{status:?}"
        );
    }

    // --- remote change (P4.6) ---

    fn two_file_package(
        state: &mut EngineState,
        now: Instant,
    ) -> (
        crate::model::PackageId,
        [crate::model::FileId; 2],
        Vec<Effect>,
    ) {
        let urls = vec![
            "http://a.example.com/a.bin".to_string(),
            "http://b.example.com/b.bin".to_string(),
        ];
        let (package, files, mut effects) = add_probed_package(state, now, &urls, 1000);
        for file in &files {
            effects.extend(state.handle(
                Input::FilePrepared {
                    file: *file,
                    result: prepared("x.bin"),
                },
                now,
            ));
        }
        (package, [files[0], files[1]], effects)
    }

    #[test]
    fn a_changed_file_fails_alone_and_the_rest_of_the_package_carries_on() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![link(1, "a0", 4)]), now);
        let (package, [fa, fb], effects) = two_file_package(&mut state, now);
        let (attempt_a, range_a) = find_start_attempt(&effects, fa).unwrap();
        let (attempt_b, _) = find_start_attempt(&effects, fb).unwrap();

        let effects = state.handle(
            Input::AttemptEnded {
                id: attempt_a,
                outcome: AttemptOutcome::Failed(TransferError::RemoteChanged(
                    "size changed".to_string(),
                )),
                position: range_a.start,
            },
            now,
        );
        check_invariants(&state);
        assert!(matches!(
            state.file(fa).map(|f| f.status.clone()),
            Some(FileStatus::Failed(
                crate::model::FileError::RemoteChanged { .. }
            ))
        ));
        // Only that file: B is untouched and the package is still going.
        assert_eq!(
            state.file(fb).map(|f| f.status.clone()),
            Some(FileStatus::Active)
        );
        assert_eq!(
            state.package(package).map(|p| p.status.clone()),
            Some(PackageStatus::Downloading)
        );
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::AbortAttempt { .. }))
        );

        // B finishes; now every file has settled → the package fails.
        finish(&mut state, now, attempt_b, 1000);
        assert_eq!(
            state.package(package).map(|p| p.status.clone()),
            Some(PackageStatus::Failed(
                crate::model::PackageError::FilesFailed
            ))
        );
    }

    #[test]
    fn a_failed_file_stops_its_other_blocks() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![link(1, "a0", 4)]), now);
        let (_, file) = add_single_file(&mut state, now, "http://big.example.com/big.bin");
        state.handle(
            Input::ProbeDone {
                file,
                result: ok_probe("http://big.example.com/big.bin", 64 * 1024 * 1024),
            },
            now,
        );
        let effects = state.handle(
            Input::FilePrepared {
                file,
                result: prepared("big.bin"),
            },
            now,
        );
        let all: Vec<_> = effects
            .iter()
            .filter_map(|e| match e {
                Effect::StartAttempt { id, range, .. } => Some((*id, range.start)),
                _ => None,
            })
            .collect();
        assert_eq!(all.len(), 4);
        let effects = state.handle(
            Input::AttemptEnded {
                id: all[0].0,
                outcome: AttemptOutcome::Failed(TransferError::RemoteChanged("changed".into())),
                position: all[0].1,
            },
            now,
        );
        let aborted = effects
            .iter()
            .filter(|e| matches!(e, Effect::AbortAttempt { .. }))
            .count();
        assert_eq!(aborted, 3, "the file's other running blocks are stopped");
        assert_eq!(state.attempt_count(), 0);
    }

    #[test]
    fn restarting_a_failed_file_resets_it_and_revives_the_package() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![link(1, "a0", 4)]), now);
        let (package, [fa, fb], effects) = two_file_package(&mut state, now);
        let (attempt_a, range_a) = find_start_attempt(&effects, fa).unwrap();
        let (attempt_b, _) = find_start_attempt(&effects, fb).unwrap();
        state.handle(
            Input::AttemptEnded {
                id: attempt_a,
                outcome: AttemptOutcome::Failed(TransferError::RemoteChanged("x".into())),
                position: range_a.start,
            },
            now,
        );
        finish(&mut state, now, attempt_b, 1000);
        assert!(matches!(
            state.package(package).map(|p| p.status.clone()),
            Some(PackageStatus::Failed(_))
        ));

        let effects = state.handle(Input::Command(Command::RestartFile(fa)), now);
        check_invariants(&state);
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::DiscardFile { file, .. } if *file == fa))
        );
        assert!(effects.contains(&Effect::Emit(EngineEvent::FileRestarted { file: fa })));
        let entry = state.file(fa).unwrap();
        assert_eq!(entry.status, FileStatus::Unprobed);
        assert!(entry.parts.is_empty() && entry.url.is_none() && entry.versions.is_empty());
        assert_eq!(
            state.package(package).map(|p| p.status.clone()),
            Some(PackageStatus::Downloading)
        );
        // The probe pool takes it from here; B (verified) is untouched.
        assert!(probes(&effects).contains(&fa));
        assert_eq!(
            state.file(fb).map(|f| f.status.clone()),
            Some(FileStatus::Verified)
        );
    }

    #[test]
    fn restarting_a_file_that_has_not_failed_does_nothing() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![link(1, "a0", 4)]), now);
        let (_, [fa, _], _) = two_file_package(&mut state, now);
        assert!(
            state
                .handle(Input::Command(Command::RestartFile(fa)), now)
                .is_empty()
        );
    }

    #[test]
    fn a_confirmed_relabelling_adds_the_version_once() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![link(1, "a0", 4)]), now);
        let (_, file) = add_single_file(&mut state, now, "http://a.example.com/f.bin");
        activate_and_start(&mut state, now, file, "http://a.example.com/f.bin", 1000);
        let relabelled = crate::model::FileVersion {
            etag: Some("\"other\"".to_string()),
            last_modified: None,
            total_bytes: 1000,
        };
        for _ in 0..2 {
            state.handle(
                Input::VersionAccepted {
                    file,
                    version: relabelled.clone(),
                },
                now,
            );
        }
        assert_eq!(state.file(file).unwrap().versions.len(), 2);
    }

    // --- post-processing: verify, extract (P5.5) ---

    use crate::model::{ArchiveScheme, ArchiveSet, PackageError, PostOptions};
    use crate::verify::{VerifyFailure, VerifyReport};

    fn rar_set(base: &str) -> ArchiveSet {
        ArchiveSet {
            scheme: ArchiveScheme::Rar,
            base_name: base.to_string(),
        }
    }

    fn extract_post(base: &str, delete: bool) -> PostOptions {
        PostOptions {
            archive: Some(rar_set(base)),
            extract: true,
            delete_archives: delete,
        }
    }

    /// Adds a package of empty files named `names` and drives every file
    /// to `Verified` (an empty file completes on activation). Returns the
    /// package and every effect produced on the way.
    fn finished_package(
        state: &mut EngineState,
        now: Instant,
        names: &[&str],
        post: PostOptions,
    ) -> (crate::model::PackageId, Vec<Effect>) {
        let mut all = state.handle(
            Input::Command(Command::AddPackage {
                name: "pkg".to_string(),
                dest_dir: std::path::PathBuf::from("/tmp/nuxd-sim"),
                sources: names
                    .iter()
                    .map(|n| format!("http://example.com/{n}"))
                    .collect(),
                priority: Priority::Normal,
                post,
            }),
            now,
        );
        let package = package_added(&all).unwrap_or_else(|| panic!("no PackageAdded"));
        let files: Vec<_> = state
            .package(package)
            .unwrap()
            .files
            .iter()
            .map(|f| f.id)
            .collect();
        for (file, name) in files.iter().zip(names) {
            all.extend(state.handle(
                Input::ProbeDone {
                    file: *file,
                    result: ok_probe(&format!("http://example.com/{name}"), 0),
                },
                now,
            ));
        }
        let mut prepared = std::collections::HashSet::new();
        loop {
            let ready: Vec<_> = all
                .iter()
                .filter_map(|e| match e {
                    Effect::PrepareFile { file, .. } if !prepared.contains(file) => Some(*file),
                    _ => None,
                })
                .collect();
            if ready.is_empty() {
                break;
            }
            for file in ready {
                prepared.insert(file);
                let index = files.iter().position(|f| *f == file).unwrap();
                all.extend(state.handle(
                    Input::FilePrepared {
                        file,
                        result: Ok(PreparedFile {
                            final_name: names[index].to_string(),
                            frontiers_valid: true,
                        }),
                    },
                    now,
                ));
                check_invariants(state);
            }
        }
        (package, all)
    }

    fn verified_ok(
        state: &mut EngineState,
        now: Instant,
        package: crate::model::PackageId,
    ) -> Vec<Effect> {
        let effects = state.handle(
            Input::VerifyDone {
                package,
                result: Ok(VerifyReport::default()),
            },
            now,
        );
        check_invariants(state);
        effects
    }

    fn extract_effect(
        effects: &[Effect],
        package: crate::model::PackageId,
    ) -> Option<&crate::extract::ExtractSpec> {
        effects.iter().find_map(|e| match e {
            Effect::Extract { package: p, spec } if *p == package => Some(spec),
            _ => None,
        })
    }

    #[test]
    fn a_plain_package_is_verified_then_done() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![one_link()]), now);
        let (package, effects) =
            finished_package(&mut state, now, &["a.bin", "b.bin"], PostOptions::default());
        assert_eq!(
            state.package(package).unwrap().status,
            PackageStatus::Verifying
        );
        let verify = effects
            .iter()
            .find_map(|e| match e {
                Effect::VerifyPackage { files, .. } => Some(files.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            verify.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
            vec!["a.bin", "b.bin"]
        );
        let effects = verified_ok(&mut state, now, package);
        assert_eq!(state.package(package).unwrap().status, PackageStatus::Done);
        assert!(effects.contains(&Effect::Emit(EngineEvent::PackageDone { package })));
        assert!(!effects.iter().any(|e| matches!(e, Effect::Extract { .. })));
    }

    #[test]
    fn extract_runs_after_verification_with_volumes_in_reading_order() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![one_link()]), now);
        // Files arrive in a shuffled order, with a checksum sidecar.
        let (package, _) = finished_package(
            &mut state,
            now,
            &["x.part2.rar", "x.sfv", "x.part1.rar"],
            extract_post("x", false),
        );
        assert_eq!(
            state.package(package).unwrap().status,
            PackageStatus::Verifying
        );
        let effects = verified_ok(&mut state, now, package);
        assert_eq!(
            state.package(package).unwrap().status,
            PackageStatus::Extracting
        );
        assert!(effects.contains(&Effect::Emit(EngineEvent::PackageExtracting { package })));
        let spec = extract_effect(&effects, package).expect("Extract effect");
        let names: Vec<_> = spec
            .volumes
            .iter()
            .map(|v| v.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        // Volume order, and the sidecar is not a volume.
        assert_eq!(names, vec!["x.part1.rar", "x.part2.rar"]);
        assert_eq!(spec.scheme, ArchiveScheme::Rar);

        let effects = state.handle(
            Input::ExtractDone {
                package,
                result: Ok(Default::default()),
            },
            now,
        );
        check_invariants(&state);
        assert_eq!(state.package(package).unwrap().status, PackageStatus::Done);
        assert!(effects.contains(&Effect::Emit(EngineEvent::PackageDone { package })));
        // "Delete archives" was off: nothing is deleted.
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::DeleteFiles { .. }))
        );
    }

    #[test]
    fn archives_are_deleted_only_when_asked_and_only_after_success() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![one_link()]), now);
        let (package, _) = finished_package(
            &mut state,
            now,
            &["x.part1.rar", "x.part2.rar", "x.sfv"],
            extract_post("x", true),
        );
        let effects = verified_ok(&mut state, now, package);
        // Nothing is deleted just for being verified.
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::DeleteFiles { .. }))
        );

        let effects = state.handle(
            Input::ExtractDone {
                package,
                result: Ok(Default::default()),
            },
            now,
        );
        let deleted = effects
            .iter()
            .find_map(|e| match e {
                Effect::DeleteFiles { names, .. } => Some(names.clone()),
                _ => None,
            })
            .expect("DeleteFiles");
        // The volumes, but not the checksum sidecar.
        assert_eq!(deleted, vec!["x.part1.rar", "x.part2.rar"]);

        // A failed extraction deletes nothing.
        let (second, _) = finished_package(
            &mut state,
            now,
            &["y.part1.rar", "y.part2.rar"],
            extract_post("y", true),
        );
        verified_ok(&mut state, now, second);
        let effects = state.handle(
            Input::ExtractDone {
                package: second,
                result: Err("boom".to_string()),
            },
            now,
        );
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::DeleteFiles { .. }))
        );
    }

    #[test]
    fn the_extract_option_is_ignored_for_a_non_archive() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![one_link()]), now);
        let post = PostOptions {
            archive: None,
            extract: true,
            delete_archives: false,
        };
        let (package, _) = finished_package(&mut state, now, &["a.bin"], post);
        assert!(!state.package(package).unwrap().extract);
        let effects = verified_ok(&mut state, now, package);
        assert_eq!(state.package(package).unwrap().status, PackageStatus::Done);
        assert!(!effects.iter().any(|e| matches!(e, Effect::Extract { .. })));
    }

    #[test]
    fn extraction_is_one_at_a_time_and_overlaps_the_next_download() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![one_link()]), now);
        let (first, _) =
            finished_package(&mut state, now, &["a.part1.rar"], extract_post("a", false));
        let (second, _) =
            finished_package(&mut state, now, &["b.part1.rar"], extract_post("b", false));
        let effects = verified_ok(&mut state, now, first);
        assert!(extract_effect(&effects, first).is_some());

        // A third package is still downloading while the first extracts.
        let effects = state.handle(
            Input::Command(Command::AddPackage {
                name: "next".to_string(),
                dest_dir: std::path::PathBuf::from("/tmp/nuxd-sim"),
                sources: vec!["http://example.com/next.bin".to_string()],
                priority: Priority::Normal,
                post: Default::default(),
            }),
            now,
        );
        let third = package_added(&effects).unwrap();
        assert_eq!(
            state.package(first).unwrap().status,
            PackageStatus::Extracting
        );
        assert_eq!(
            state.package(third).unwrap().status,
            PackageStatus::Downloading
        );

        // The second is verified while the first extracts: it waits.
        let effects = verified_ok(&mut state, now, second);
        assert!(extract_effect(&effects, second).is_none());
        assert_eq!(
            state.package(second).unwrap().status,
            PackageStatus::Verifying
        );

        // The first finishing starts the second.
        let effects = state.handle(
            Input::ExtractDone {
                package: first,
                result: Ok(Default::default()),
            },
            now,
        );
        assert!(extract_effect(&effects, second).is_some());
        assert_eq!(
            state.package(second).unwrap().status,
            PackageStatus::Extracting
        );
        assert_eq!(state.package(first).unwrap().status, PackageStatus::Done);
    }

    #[test]
    fn a_failed_extraction_can_be_retried_with_resume() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![one_link()]), now);
        let (package, _) =
            finished_package(&mut state, now, &["a.part1.rar"], extract_post("a", false));
        verified_ok(&mut state, now, package);
        let effects = state.handle(
            Input::ExtractDone {
                package,
                result: Err("disk full".to_string()),
            },
            now,
        );
        check_invariants(&state);
        assert_eq!(
            state.package(package).unwrap().status,
            PackageStatus::Failed(PackageError::ExtractFailed {
                detail: "disk full".to_string()
            })
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::Emit(EngineEvent::PackageFailed { .. })))
        );

        let effects = state.handle(Input::Command(Command::Resume(package)), now);
        assert_eq!(
            state.package(package).unwrap().status,
            PackageStatus::Verifying
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::VerifyPackage { .. }))
        );
    }

    #[test]
    fn a_corrupt_file_is_deleted_and_can_be_restarted() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![one_link()]), now);
        let (package, _) =
            finished_package(&mut state, now, &["a.bin", "b.bin"], PostOptions::default());
        let bad = state.package(package).unwrap().files[1].id;
        let effects = state.handle(
            Input::VerifyDone {
                package,
                result: Err(VerifyFailure {
                    file: Some("b.bin".to_string()),
                    corrupt: true,
                    detail: "b.bin: CRC32 mismatch".to_string(),
                }),
            },
            now,
        );
        let pkg = state.package(package).unwrap();
        assert!(matches!(
            pkg.status,
            PackageStatus::Failed(PackageError::VerifyFailed { .. })
        ));
        assert!(matches!(
            pkg.files[1].status,
            FileStatus::Failed(crate::model::FileError::VerifyFailed { .. })
        ));
        assert_eq!(pkg.files[0].status, FileStatus::Verified);
        assert!(effects.iter().any(
            |e| matches!(e, Effect::DeleteFiles { names, .. } if names == &vec!["b.bin".to_string()])
        ));

        // Restarting the file revives the package.
        state.handle(Input::Command(Command::RestartFile(bad)), now);
        check_invariants(&state);
        assert_eq!(
            state.package(package).unwrap().status,
            PackageStatus::Downloading
        );
    }

    #[test]
    fn a_failure_with_no_corrupt_file_just_fails_the_package() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![one_link()]), now);
        let (package, _) = finished_package(&mut state, now, &["a.bin"], PostOptions::default());
        let effects = state.handle(
            Input::VerifyDone {
                package,
                result: Err(VerifyFailure {
                    file: None,
                    corrupt: false,
                    detail: "unreadable".to_string(),
                }),
            },
            now,
        );
        assert!(matches!(
            state.package(package).unwrap().status,
            PackageStatus::Failed(PackageError::VerifyFailed { .. })
        ));
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::DeleteFiles { .. }))
        );
        assert_eq!(
            state.package(package).unwrap().files[0].status,
            FileStatus::Verified
        );
    }

    #[test]
    fn cancelling_an_extracting_package_stops_it_and_starts_the_next() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![one_link()]), now);
        let (first, _) =
            finished_package(&mut state, now, &["a.part1.rar"], extract_post("a", false));
        let (second, _) =
            finished_package(&mut state, now, &["b.part1.rar"], extract_post("b", false));
        verified_ok(&mut state, now, first);
        verified_ok(&mut state, now, second);

        let effects = state.handle(Input::Command(Command::Cancel(first)), now);
        assert!(effects.contains(&Effect::CancelPost { package: first }));
        assert!(extract_effect(&effects, second).is_some());
        // The cancelled run's late result changes nothing.
        let status_before = state.package(first).unwrap().status.clone();
        state.handle(
            Input::ExtractDone {
                package: first,
                result: Ok(Default::default()),
            },
            now,
        );
        assert_eq!(state.package(first).unwrap().status, status_before);
        assert!(matches!(status_before, PackageStatus::Failed(_)));
        assert_eq!(
            state.package(second).unwrap().status,
            PackageStatus::Extracting
        );
    }

    #[test]
    fn removing_a_package_waiting_for_extraction_drops_it_from_the_queue() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![one_link()]), now);
        let (first, _) =
            finished_package(&mut state, now, &["a.part1.rar"], extract_post("a", false));
        let (second, _) =
            finished_package(&mut state, now, &["b.part1.rar"], extract_post("b", false));
        verified_ok(&mut state, now, first);
        verified_ok(&mut state, now, second);
        let effects = state.handle(Input::Command(Command::Remove(second)), now);
        assert!(effects.contains(&Effect::CancelPost { package: second }));
        let effects = state.handle(
            Input::ExtractDone {
                package: first,
                result: Ok(Default::default()),
            },
            now,
        );
        assert!(!effects.iter().any(|e| matches!(e, Effect::Extract { .. })));
    }

    #[test]
    fn the_options_can_be_changed_before_the_package_finishes() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![one_link()]), now);
        let post = PostOptions {
            archive: Some(rar_set("a")),
            extract: false,
            delete_archives: false,
        };
        let effects = state.handle(
            Input::Command(Command::AddPackage {
                name: "a".to_string(),
                dest_dir: std::path::PathBuf::from("/tmp/nuxd-sim"),
                sources: vec!["http://example.com/a.part1.rar".to_string()],
                priority: Priority::Normal,
                post,
            }),
            now,
        );
        let package = package_added(&effects).unwrap();
        let effects = state.handle(
            Input::Command(Command::SetPostOptions {
                package,
                extract: true,
                delete_archives: true,
            }),
            now,
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::Checkpoint { .. }))
        );
        let pkg = state.package(package).unwrap();
        assert!(pkg.extract && pkg.delete_archives);
    }

    #[test]
    fn restoring_a_package_that_was_mid_post_processing_verifies_it_again() {
        use crate::storage::manifest::QueueManifest;
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![one_link()]), now);
        let (package, _) =
            finished_package(&mut state, now, &["a.part1.rar"], extract_post("a", false));
        verified_ok(&mut state, now, package);
        assert_eq!(
            state.package(package).unwrap().status,
            PackageStatus::Extracting
        );

        let snapshot = state.snapshot();
        let queue = QueueManifest {
            schema_version: 1,
            next_package_id: snapshot.next_package_id,
            next_file_id: snapshot.next_file_id,
            package_order: snapshot.packages.iter().map(|p| p.id).collect(),
            settings: Default::default(),
        };
        let (restored, effects) =
            EngineState::restore(EngineConfig::default(), &queue, snapshot.packages);
        assert_eq!(
            restored.package(package).unwrap().status,
            PackageStatus::Verifying
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::VerifyPackage { .. }))
        );
    }

    #[test]
    fn late_post_results_for_unknown_or_wrong_state_packages_are_ignored() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![one_link()]), now);
        // Unknown package.
        let ghost = crate::model::PackageId::new(99);
        state.handle(
            Input::VerifyDone {
                package: ghost,
                result: Ok(VerifyReport::default()),
            },
            now,
        );
        state.handle(
            Input::ExtractDone {
                package: ghost,
                result: Ok(Default::default()),
            },
            now,
        );
        // A package that is still downloading never becomes Done from a stray result.
        let (package, _) = add_single_file(&mut state, now, "http://example.com/f.bin");
        state.handle(
            Input::VerifyDone {
                package,
                result: Ok(VerifyReport::default()),
            },
            now,
        );
        assert_eq!(
            state.package(package).unwrap().status,
            PackageStatus::Downloading
        );
    }

    #[test]
    fn a_package_limit_is_applied_checkpointed_and_announced_once() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![one_link()]), now);
        let (package, _) = add_single_file(&mut state, now, "http://example.com/f.bin");
        let limit = crate::model::ByteRate::new(2048);
        let effects = state.handle(
            Input::Command(Command::SetPackageLimit { package, limit }),
            now,
        );
        assert!(effects.contains(&Effect::SetPackageLimit { package, limit }));
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::Checkpoint { .. }))
        );
        assert!(effects.contains(&Effect::Emit(EngineEvent::PackageLimitChanged { package })));
        assert_eq!(state.package(package).unwrap().limit, limit);

        // Setting the same limit again is a no-op.
        let effects = state.handle(
            Input::Command(Command::SetPackageLimit { package, limit }),
            now,
        );
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::SetPackageLimit { .. }))
        );

        let effects = state.handle(
            Input::Command(Command::SetPackageLimit {
                package,
                limit: None,
            }),
            now,
        );
        assert!(effects.contains(&Effect::SetPackageLimit {
            package,
            limit: None
        }));
    }

    // --- hedging (P7.1, §7.5) ---

    fn two_links() -> Vec<crate::model::Link> {
        let mut second = one_link();
        second.id = LinkId::new(2);
        second.name = "sim1".to_string();
        vec![one_link(), second]
    }

    fn starts_for(
        effects: &[Effect],
        file: crate::model::FileId,
    ) -> Vec<(crate::model::AttemptId, crate::model::ByteRange, LinkId)> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::StartAttempt {
                    id,
                    file: f,
                    range,
                    link,
                    ..
                } if *f == file => Some((*id, *range, *link)),
                _ => None,
            })
            .collect()
    }

    fn lost(effects: &[Effect], attempt: crate::model::AttemptId) -> bool {
        effects.contains(&Effect::AbortAttempt {
            id: attempt,
            reason: AbortReason::Lost,
        })
    }

    /// One crawling 1000-byte single-block file on link 1, ticked to
    /// `secs` with `progress` bytes delivered. Returns the state, the
    /// file, the holder and the tick's effects.
    fn crawling(
        links: Vec<crate::model::Link>,
        secs: u64,
        progress: u64,
    ) -> (
        EngineState,
        crate::model::PackageId,
        crate::model::FileId,
        crate::model::AttemptId,
        Instant,
        Vec<Effect>,
    ) {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(links), now);
        let (package, file) = add_single_file(&mut state, now, "http://example.com/f.bin");
        let (holder, _) =
            activate_and_start(&mut state, now, file, "http://example.com/f.bin", 1000);
        let later = now + Duration::from_secs(secs);
        let effects = state.handle(
            Input::Tick {
                progress: vec![(holder, progress)],
            },
            later,
        );
        check_invariants(&state);
        (state, package, file, holder, later, effects)
    }

    #[test]
    fn a_crawling_block_is_finished_by_a_hedge() {
        let (mut state, package, file, holder, later, effects) = crawling(two_links(), 9, 10);
        let hedges = starts_for(&effects, file);
        assert_eq!(hedges.len(), 1, "{effects:?}");
        let (hedge, range, link) = hedges[0];
        // From the durable frontier, on the other link.
        assert_eq!(range.start, 10);
        assert_eq!(range.end, Some(999));
        assert_eq!(link, LinkId::new(2));

        let effects = state.handle(
            Input::AttemptEnded {
                id: hedge,
                outcome: AttemptOutcome::Success,
                position: 1000,
            },
            later + Duration::from_secs(1),
        );
        check_invariants(&state);
        assert!(
            lost(&effects, holder),
            "the slow holder must lose: {effects:?}"
        );
        let pkg = state.package(package).unwrap();
        assert_eq!(pkg.files[0].status, FileStatus::Verified);

        // The loser's late report changes nothing.
        let effects = state.handle(
            Input::AttemptEnded {
                id: holder,
                outcome: AttemptOutcome::Success,
                position: 50,
            },
            later + Duration::from_secs(2),
        );
        assert!(
            effects.is_empty()
                || !effects
                    .iter()
                    .any(|e| matches!(e, Effect::StartAttempt { .. }))
        );
    }

    #[test]
    fn the_holder_can_win_the_race_too() {
        let (mut state, _, file, holder, later, effects) = crawling(two_links(), 9, 10);
        let (hedge, _, _) = starts_for(&effects, file)[0];
        let effects = state.handle(
            Input::AttemptEnded {
                id: holder,
                outcome: AttemptOutcome::Success,
                position: 1000,
            },
            later,
        );
        assert!(lost(&effects, hedge));
    }

    #[test]
    fn no_hedge_before_hedge_after_or_for_a_holder_nearly_done() {
        // Not yet `hedge_after` (1 s) old.
        let (_, _, file, _, _, effects) = crawling(two_links(), 0, 0);
        assert!(starts_for(&effects, file).is_empty());
        // 990 of 1000 bytes in 9 s: a moment left — not worth racing.
        let (_, _, file, _, _, effects) = crawling(two_links(), 9, 990);
        assert!(starts_for(&effects, file).is_empty());
    }

    #[test]
    fn no_hedge_while_pending_work_exists() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(two_links()), now);
        let (_, file) = add_single_file(&mut state, now, "http://example.com/f.bin");
        let (holder, _) =
            activate_and_start(&mut state, now, file, "http://example.com/f.bin", 1000);
        // A second file is still waiting for its probe: real work remains.
        let (_, waiting) = add_single_file(&mut state, now, "http://example.com/g.bin");
        let effects = state.handle(
            Input::Tick {
                progress: vec![(holder, 10)],
            },
            now + Duration::from_secs(30),
        );
        assert!(starts_for(&effects, file).is_empty(), "{effects:?}");
        assert!(starts_for(&effects, waiting).is_empty());
    }

    #[test]
    fn a_hedge_on_a_single_link_uses_the_holders_link() {
        let (_, _, file, _, _, effects) = crawling(vec![one_link()], 9, 10);
        let hedges = starts_for(&effects, file);
        assert_eq!(hedges.len(), 1);
        assert_eq!(hedges[0].2, LinkId::new(1));
    }

    #[test]
    fn failed_hedges_cost_nothing_and_the_budget_is_two() {
        let (mut state, _, file, holder, later, effects) = crawling(two_links(), 9, 10);
        let (first, _, _) = starts_for(&effects, file)[0];
        let effects = state.handle(
            Input::AttemptEnded {
                id: first,
                outcome: AttemptOutcome::Failed(TransferError::Reset),
                position: 10,
            },
            later,
        );
        check_invariants(&state);
        // No retry backoff for the block; a second hedge goes out at once.
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::WakeAt(_))),
            "{effects:?}"
        );
        let second = starts_for(&effects, file);
        assert_eq!(second.len(), 1, "{effects:?}");
        let effects = state.handle(
            Input::AttemptEnded {
                id: second[0].0,
                outcome: AttemptOutcome::Failed(TransferError::Reset),
                position: 10,
            },
            later,
        );
        // Budget spent: no third hedge, and the holder is untouched.
        assert!(starts_for(&effects, file).is_empty(), "{effects:?}");
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::AbortAttempt { id, .. } if *id == holder))
        );
    }

    #[test]
    fn a_failing_holder_leaves_the_block_to_its_hedge() {
        let (mut state, package, file, holder, later, effects) = crawling(two_links(), 9, 10);
        let (hedge, _, _) = starts_for(&effects, file)[0];
        let effects = state.handle(
            Input::AttemptEnded {
                id: holder,
                outcome: AttemptOutcome::Failed(TransferError::Reset),
                position: 10,
            },
            later,
        );
        // Nothing new starts for the block: the hedge still runs it.
        assert!(starts_for(&effects, file).is_empty(), "{effects:?}");
        let effects = state.handle(
            Input::AttemptEnded {
                id: hedge,
                outcome: AttemptOutcome::Success,
                position: 1000,
            },
            later,
        );
        check_invariants(&state);
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::AbortAttempt { .. }))
        );
        assert_eq!(
            state.package(package).unwrap().files[0].status,
            FileStatus::Verified
        );
    }

    #[test]
    fn a_server_without_ranges_is_never_raced() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(two_links()), now);
        let (_, file) = add_single_file(&mut state, now, "http://example.com/f.bin");
        let mut probe = ok_probe("http://example.com/f.bin", 1000).unwrap();
        probe.supports_ranges = false;
        state.handle(
            Input::ProbeDone {
                file,
                result: Ok(probe),
            },
            now,
        );
        let effects = state.handle(
            Input::FilePrepared {
                file,
                result: Ok(PreparedFile {
                    final_name: "f.bin".to_string(),
                    frontiers_valid: true,
                }),
            },
            now,
        );
        let (holder, _) = find_start_attempt(&effects, file).unwrap();
        let effects = state.handle(
            Input::Tick {
                progress: vec![(holder, 10)],
            },
            now + Duration::from_secs(30),
        );
        assert!(starts_for(&effects, file).is_empty());
    }

    // --- slow-connection refresh (P7.2, §7.5) ---

    /// Hedging would race a slow block before the refresh got to it.
    fn no_hedging() -> EngineConfig {
        EngineConfig {
            max_hedges_per_block: 0,
            ..EngineConfig::default()
        }
    }

    /// `n` one-block files, each with a running attempt on the first link
    /// (or alternating when `links` has two). Returns the state and
    /// `(file, attempt)` pairs.
    fn running(
        links: Vec<crate::model::Link>,
        n: usize,
    ) -> (
        EngineState,
        Vec<(crate::model::FileId, crate::model::AttemptId)>,
        Instant,
    ) {
        let mut state = EngineState::new(no_hedging());
        let now = Instant::now();
        state.handle(Input::LinksChanged(links), now);
        let mut out = Vec::new();
        for i in 0..n {
            let url = format!("http://example.com/{i}.bin");
            let (_, file) = add_single_file(&mut state, now, &url);
            let (attempt, _) = activate_and_start(&mut state, now, file, &url, 3_000_000);
            out.push((file, attempt));
        }
        (state, out, now)
    }

    fn refreshed(effects: &[Effect], attempt: crate::model::AttemptId) -> bool {
        effects.contains(&Effect::AbortAttempt {
            id: attempt,
            reason: AbortReason::Refresh,
        })
    }

    /// Ticks once a second for `secs` seconds. `speeds[i]` bytes/s is the
    /// pace of attempt i (0 = silent). Returns every refreshed attempt.
    fn run_ticks(
        state: &mut EngineState,
        attempts: &[(crate::model::FileId, crate::model::AttemptId)],
        speeds: &[u64],
        start: Instant,
        from_sec: u64,
        to_sec: u64,
        refreshed_ids: &mut Vec<crate::model::AttemptId>,
    ) {
        for sec in from_sec..=to_sec {
            let progress = attempts
                .iter()
                .zip(speeds)
                .filter(|((_, id), _)| !refreshed_ids.contains(id))
                .map(|((_, id), speed)| (*id, speed * sec))
                .collect();
            let effects = state.handle(Input::Tick { progress }, start + Duration::from_secs(sec));
            check_invariants(state);
            for (_, id) in attempts {
                if refreshed(&effects, *id) {
                    refreshed_ids.push(*id);
                }
            }
        }
    }

    #[test]
    fn a_connection_far_below_its_links_median_is_refreshed_after_the_grace_periods() {
        let (mut state, attempts, start) = running(vec![one_link()], 3);
        let mut done = Vec::new();
        // Attempt 0 crawls at 1 kB/s; the others run at 500 kB/s.
        run_ticks(
            &mut state,
            &attempts,
            &[100, 10_000, 10_000],
            start,
            1,
            7,
            &mut done,
        );
        assert!(done.is_empty(), "too early: {done:?}");
        run_ticks(
            &mut state,
            &attempts,
            &[100, 10_000, 10_000],
            start,
            8,
            12,
            &mut done,
        );
        assert_eq!(done, vec![attempts[0].1], "only the crawler is refreshed");
    }

    #[test]
    fn the_refreshed_block_resumes_from_its_frontier_and_is_not_a_retry() {
        let (mut state, attempts, start) = running(vec![one_link()], 3);
        let mut done = Vec::new();
        let mut restart = None;
        for sec in 1..=12u64 {
            let progress = attempts
                .iter()
                .enumerate()
                .map(|(i, (_, id))| (*id, if i == 0 { 100 * sec } else { 10_000 * sec }))
                .collect();
            let effects = state.handle(Input::Tick { progress }, start + Duration::from_secs(sec));
            if refreshed(&effects, attempts[0].1) {
                restart = Some(starts_for(&effects, attempts[0].0));
                done.push(sec);
                break;
            }
        }
        let restart = restart.expect("never refreshed");
        // A new attempt for the same block, from the last ticked position.
        assert_eq!(restart.len(), 1, "{restart:?}");
        assert_eq!(restart[0].1.start, 100 * done[0]);
        // The restart is immediate: no backoff, no retry was spent.
    }

    #[test]
    fn a_silent_connection_is_refreshed_while_others_progress() {
        // Alone on its own link (so nothing on it to compare with), the
        // connection is judged by silence alone.
        let mut state = EngineState::new(no_hedging());
        let now = Instant::now();
        let mut lone = one_link();
        lone.max_slots = 1;
        let mut other = one_link();
        other.id = LinkId::new(2);
        other.name = "other".to_string();
        state.handle(Input::LinksChanged(vec![lone, other]), now);
        let mut attempts = Vec::new();
        for i in 0..2 {
            let url = format!("http://example.com/{i}.bin");
            let (_, file) = add_single_file(&mut state, now, &url);
            let (attempt, _) = activate_and_start(&mut state, now, file, &url, 3_000_000);
            attempts.push((file, attempt));
        }
        // Work still waiting elsewhere keeps hedging (which would race the
        // silent block instead) out of the picture.
        add_single_file(&mut state, now, "http://example.com/waiting.bin");
        let mut done = Vec::new();
        run_ticks(&mut state, &attempts, &[0, 10_000], now, 1, 9, &mut done);
        assert!(done.is_empty(), "{done:?}");
        run_ticks(&mut state, &attempts, &[0, 10_000], now, 10, 12, &mut done);
        assert_eq!(done, vec![attempts[0].1]);
    }

    #[test]
    fn a_silent_connection_beside_its_peers_is_caught_as_crawling() {
        let (mut state, attempts, start) = running(vec![one_link()], 2);
        let mut done = Vec::new();
        run_ticks(&mut state, &attempts, &[0, 10_000], start, 1, 12, &mut done);
        assert_eq!(done, vec![attempts[0].1]);
    }

    #[test]
    fn nothing_is_refreshed_when_everything_is_silent() {
        // A dead network is the stall timeout's job, not the refresh's.
        let (mut state, attempts, start) = running(vec![one_link()], 3);
        let mut done = Vec::new();
        run_ticks(&mut state, &attempts, &[0, 0, 0], start, 1, 40, &mut done);
        assert!(done.is_empty(), "{done:?}");
    }

    #[test]
    fn links_are_never_compared_with_each_other() {
        // A slow cellular connection alone on its link, beside fast
        // Ethernet ones, is exactly as expected.
        let mut state = EngineState::new(no_hedging());
        let now = Instant::now();
        let mut slow_link = one_link();
        slow_link.max_slots = 1;
        let mut fast_link = one_link();
        fast_link.id = LinkId::new(2);
        fast_link.name = "fast".to_string();
        state.handle(Input::LinksChanged(vec![slow_link, fast_link]), now);
        let mut attempts = Vec::new();
        for i in 0..3 {
            let url = format!("http://example.com/{i}.bin");
            let (_, file) = add_single_file(&mut state, now, &url);
            let (attempt, _) = activate_and_start(&mut state, now, file, &url, 3_000_000);
            attempts.push((file, attempt));
        }
        let mut done = Vec::new();
        run_ticks(
            &mut state,
            &attempts,
            &[100, 10_000, 10_000],
            now,
            1,
            30,
            &mut done,
        );
        // The first attempt sits on link 1 (only one slot) and nothing
        // else is on that link.
        assert!(done.is_empty(), "{done:?}");
    }

    #[test]
    fn a_rate_limited_package_is_not_mistaken_for_a_crawler() {
        let (mut state, attempts, start) = running(vec![one_link()], 3);
        let package = state
            .packages()
            .iter()
            .find(|p| p.files[0].id == attempts[0].0)
            .unwrap()
            .id;
        state.handle(
            Input::Command(Command::SetPackageLimit {
                package,
                limit: crate::model::ByteRate::new(100),
            }),
            start,
        );
        let mut done = Vec::new();
        run_ticks(
            &mut state,
            &attempts,
            &[100, 10_000, 10_000],
            start,
            1,
            30,
            &mut done,
        );
        assert!(done.is_empty(), "{done:?}");
    }

    #[test]
    fn a_block_is_refreshed_at_most_twice() {
        let (mut state, attempts, start) = running(vec![one_link()], 3);
        let mut refreshes = 0;
        let mut current = attempts[0].1;
        let mut sec = 1u64;
        let mut base = 0u64;
        let mut started = start;
        while sec <= 200 {
            // The crawler (whatever attempt currently holds file 0's
            // block) creeps; the others fly.
            let progress = vec![
                (current, base + 100 * (sec - (started - start).as_secs())),
                (attempts[1].1, 10_000 * sec),
                (attempts[2].1, 10_000 * sec),
            ];
            let effects = state.handle(Input::Tick { progress }, start + Duration::from_secs(sec));
            if refreshed(&effects, current) {
                refreshes += 1;
                let next = starts_for(&effects, attempts[0].0);
                if next.len() != 1 {
                    break;
                }
                base = next[0].1.start;
                current = next[0].0;
                started = start + Duration::from_secs(sec);
            }
            sec += 1;
        }
        assert_eq!(refreshes, 2);
    }

    // --- disk errors pause the package (P8.3) ---

    #[test]
    fn a_write_error_pauses_the_package_and_resume_reopens_the_file() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![one_link()]), now);
        let (package, file) = add_single_file(&mut state, now, "http://example.com/f.bin");
        let (attempt, _) =
            activate_and_start(&mut state, now, file, "http://example.com/f.bin", 1000);
        state.handle(
            Input::Tick {
                progress: vec![(attempt, 400)],
            },
            now,
        );
        let effects = state.handle(
            Input::AttemptEnded {
                id: attempt,
                outcome: AttemptOutcome::Failed(TransferError::Io(
                    "No space left on device".into(),
                )),
                position: 400,
            },
            now,
        );
        check_invariants(&state);
        let pkg = state.package(package).unwrap();
        assert_eq!(pkg.status, PackageStatus::Paused);
        assert!(
            matches!(pkg.last_error, Some(PackageError::Io { ref detail }) if detail.contains("space"))
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::Emit(EngineEvent::PackagePausedOnError { .. })))
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::Checkpoint { .. }))
        );
        // The progress made before the error is kept.
        assert_eq!(pkg.files[0].parts[0].durable, 400);
        // Nothing starts while it is paused.
        let tick = state.handle(
            Input::Tick { progress: vec![] },
            now + Duration::from_secs(30),
        );
        assert!(find_start_attempt(&tick, file).is_none());

        let effects = state.handle(Input::Command(Command::Resume(package)), now);
        check_invariants(&state);
        let pkg = state.package(package).unwrap();
        assert_eq!(pkg.status, PackageStatus::Downloading);
        assert_eq!(pkg.last_error, None);
        let prep = effects.iter().find_map(|e| match e {
            Effect::PrepareFile { file: f, plan } if *f == file => Some(plan.clone()),
            _ => None,
        });
        // Reopened by its name, not recreated.
        assert_eq!(
            prep.unwrap().resume_final_name,
            Some("file.bin".to_string())
        );
        // Back to work once prepared, from the saved byte.
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
        let (_, range) = find_start_attempt(&effects, file).expect("restarted");
        assert_eq!(range.start, 400);
    }

    #[test]
    fn a_prepare_error_pauses_too() {
        let mut state = EngineState::new(EngineConfig::default());
        let now = Instant::now();
        state.handle(Input::LinksChanged(vec![one_link()]), now);
        let (package, file) = add_single_file(&mut state, now, "http://example.com/f.bin");
        state.handle(
            Input::ProbeDone {
                file,
                result: ok_probe("http://example.com/f.bin", 1000),
            },
            now,
        );
        state.handle(
            Input::FilePrepared {
                file,
                result: Err(crate::error::StorageError::InsufficientSpace {
                    dir: "/data".to_string(),
                    needed: 1000,
                    available: 10,
                }),
            },
            now,
        );
        check_invariants(&state);
        let pkg = state.package(package).unwrap();
        assert_eq!(pkg.status, PackageStatus::Paused);
        assert!(pkg.last_error.is_some());
        // It resumes by preparing the file afresh.
        let effects = state.handle(Input::Command(Command::Resume(package)), now);
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::PrepareFile { file: f, .. } if *f == file))
        );
    }
}
