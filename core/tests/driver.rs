//! `driver::Driver` against the testkit origin (`IMPLEMENTATION_PLAN.md`
//! P2.7's done-when): the engine downloads real bytes, through real
//! sockets and real files, byte-exact.

use nuxd_core::config::EngineConfig;
use nuxd_core::driver::Driver;
use nuxd_core::engine::sim::one_link;
use nuxd_core::engine::{Command, EngineEvent, EngineState};
use nuxd_core::model::{PackageId, Priority};
use nuxd_core::storage::manifest::{
    QUEUE_SCHEMA_VERSION, QueueManifest, QueueSettings, load_package_manifest,
};
use nuxd_testkit::{Fault, Origin, OriginOptions, content_range};
use std::time::Duration;
use tempfile::tempdir;

/// Adds a single-file package and waits (bounded) for the package to
/// settle, returning whether it reached `Done`. On failure, panics with
/// whatever `FileFailed`/`PackageFailed` events were seen along the way,
/// since a bare `false` gives no clue why.
async fn run_to_completion(driver: &Driver, dest_dir: std::path::PathBuf, url: String) -> bool {
    let mut events = driver.subscribe();
    driver
        .command(Command::AddPackage {
            name: "pkg".to_string(),
            dest_dir,
            sources: vec![url],
            priority: Priority::Normal,
            post: Default::default(),
        })
        .await
        .unwrap_or_else(|error| panic!("driver: couldn't send AddPackage: {error}"));

    let mut seen = Vec::new();
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match events.recv().await {
                Ok(EngineEvent::PackageDone { .. }) => return true,
                Ok(event @ EngineEvent::PackageFailed { .. }) => {
                    seen.push(format!("{event:?}"));
                    return false;
                }
                Ok(event @ EngineEvent::FileFailed { .. }) => seen.push(format!("{event:?}")),
                Ok(_) => {}
                Err(error) => {
                    seen.push(format!("events channel error: {error}"));
                    return false;
                }
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!("driver: package never settled within the test timeout; saw {seen:?}")
    });

    if !seen.is_empty() {
        panic!("package did not complete cleanly: {seen:?}");
    }
    result
}

fn only_file_in(dir: &std::path::Path) -> std::path::PathBuf {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("couldn't read {}: {error}", dir.display()))
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .collect();
    assert_eq!(
        files.len(),
        1,
        "expected exactly one finalized file in {dir:?}, found {files:?}"
    );
    files.remove(0)
}

#[tokio::test]
async fn downloads_a_small_file_byte_exact() {
    let origin = Origin::start(OriginOptions {
        size: 5000,
        seed: 123,
        ..Default::default()
    })
    .await;
    let dest = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let manifests = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));

    let driver = Driver::spawn_fresh(
        EngineConfig::default(),
        vec![one_link()],
        manifests.path().to_path_buf(),
    );

    let done = run_to_completion(&driver, dest.path().to_path_buf(), origin.url()).await;
    assert!(done, "package did not complete successfully");

    let bytes =
        std::fs::read(only_file_in(dest.path())).unwrap_or_else(|error| panic!("read: {error}"));
    assert_eq!(bytes, content_range(123, 0, 5000));

    driver.shutdown().await;
    origin.shutdown().await;
}

#[tokio::test]
async fn downloads_a_multi_block_file_byte_exact() {
    // Comfortably over `single_block_threshold_bytes` (4 MiB), so the
    // file actually splits into several blocks the scheduler has to
    // assign one at a time on the single test link.
    let size = 6 * 1024 * 1024;
    let origin = Origin::start(OriginOptions {
        size,
        seed: 7,
        ..Default::default()
    })
    .await;
    let dest = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let manifests = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));

    let driver = Driver::spawn_fresh(
        EngineConfig::default(),
        vec![one_link()],
        manifests.path().to_path_buf(),
    );

    let done = run_to_completion(&driver, dest.path().to_path_buf(), origin.url()).await;
    assert!(done, "package did not complete successfully");

    let bytes =
        std::fs::read(only_file_in(dest.path())).unwrap_or_else(|error| panic!("read: {error}"));
    assert_eq!(bytes, content_range(7, 0, size));

    driver.shutdown().await;
    origin.shutdown().await;
}

#[tokio::test]
async fn recovers_from_a_cut_connection_and_still_finishes_byte_exact() {
    let origin = Origin::start(OriginOptions {
        size: 20_000,
        seed: 42,
        ..Default::default()
    })
    .await;
    // Request #1 is the probe (`Range: bytes=0-0`); request #2 is the
    // transfer's first attempt, cut short after 1000 bytes. On a fast
    // local connection this surfaces as a failure of the request itself
    // (`TransferError::Connect`) rather than a mid-body read error —
    // `blame()` puts that on the link, so the retry (request #3, which
    // gets the default `Ok` fault) is immediate and uncounted (§16),
    // proving the driver recovers from a connection failure end to end.
    origin.set_fault(2, Fault::CutAfter(1000));

    let dest = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let manifests = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let driver = Driver::spawn_fresh(
        EngineConfig::default(),
        vec![one_link()],
        manifests.path().to_path_buf(),
    );

    let done = run_to_completion(&driver, dest.path().to_path_buf(), origin.url()).await;
    assert!(done, "package did not recover from the cut connection");

    let bytes =
        std::fs::read(only_file_in(dest.path())).unwrap_or_else(|error| panic!("read: {error}"));
    assert_eq!(bytes, content_range(42, 0, 20_000));
    let request_count = origin.requests().len();
    assert!(
        request_count >= 3,
        "expected at least 3 requests (probe + cut attempt + retry), got {request_count}"
    );

    driver.shutdown().await;
    origin.shutdown().await;
}

#[tokio::test]
async fn recovers_from_a_short_body_with_a_backed_off_retry() {
    let origin = Origin::start(OriginOptions {
        size: 20_000,
        seed: 99,
        ..Default::default()
    })
    .await;
    // A clean short body (§16: `ShortBody`) rather than an abrupt cut —
    // this reliably fails inside the body-reading loop itself, so
    // `blame()` puts it on the attempt, which is backed off (unlike the
    // connect-level failure the cut-connection test above exercises).
    origin.set_fault(2, Fault::EndAfter(1000));

    let dest = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let manifests = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let config = EngineConfig {
        retry_base_backoff: Duration::from_millis(200),
        ..EngineConfig::default()
    };
    let driver = Driver::spawn_fresh(config, vec![one_link()], manifests.path().to_path_buf());

    let start = std::time::Instant::now();
    let done = run_to_completion(&driver, dest.path().to_path_buf(), origin.url()).await;
    assert!(done, "package did not recover from the short body");
    assert!(
        start.elapsed() >= Duration::from_millis(150),
        "recovered suspiciously fast for a backed-off retry: {:?}",
        start.elapsed()
    );

    let bytes =
        std::fs::read(only_file_in(dest.path())).unwrap_or_else(|error| panic!("read: {error}"));
    assert_eq!(bytes, content_range(99, 0, 20_000));

    driver.shutdown().await;
    origin.shutdown().await;
}

/// P2.8's `EngineState::restore` / driver `spawn` resume path (§9.4): a
/// package is paused mid-download (forcing an immediate checkpoint), the
/// driver is dropped without a graceful `shutdown()` — simulating a
/// crash right after that checkpoint rather than a clean stop — and a
/// second driver restores from the manifest and finishes the download.
#[tokio::test]
async fn resumes_from_a_checkpoint_after_a_simulated_crash() {
    let origin = Origin::start(OriginOptions {
        size: 20_000,
        seed: 55,
        ..Default::default()
    })
    .await;
    let dest = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let manifests = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    // A small write buffer and a fast tick, so a handful of flushes (and
    // the tick samples that pick them up) land well before the hold
    // point below — otherwise nothing would be durable yet to resume
    // from, and the test would prove nothing.
    let config = EngineConfig {
        write_buffer_bytes: 1000,
        tick_interval: Duration::from_millis(20),
        ..EngineConfig::default()
    };

    let package_id: PackageId;
    let durable_at_pause: u64;
    {
        let driver = Driver::spawn_fresh(
            config.clone(),
            vec![one_link()],
            manifests.path().to_path_buf(),
        );
        let mut events = driver.subscribe();

        driver
            .command(Command::AddPackage {
                name: "pkg".to_string(),
                dest_dir: dest.path().to_path_buf(),
                sources: vec![origin.url()],
                priority: Priority::Normal,
                post: Default::default(),
            })
            .await
            .unwrap_or_else(|error| panic!("command: {error}"));

        package_id = tokio::time::timeout(Duration::from_secs(5), async {
            let mut package = None;
            loop {
                match events.recv().await {
                    Ok(EngineEvent::PackageAdded { package: id }) => package = Some(id),
                    Ok(EngineEvent::FileProbed { .. }) => return package,
                    Ok(_) => {}
                    Err(_) => return None,
                }
            }
        })
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| panic!("the probe never completed"));

        // The probe (a 1-byte response) is out of the way now, so this
        // catches the transfer's request instead.
        origin.hold_next_request_at(3000);
        tokio::time::sleep(Duration::from_millis(300)).await;

        driver
            .command(Command::Pause(package_id))
            .await
            .unwrap_or_else(|error| panic!("command: {error}"));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(EngineEvent::PackagePaused { .. }) = events.recv().await {
                    return;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("pause was never confirmed"));
        // `PackagePaused` is emitted before the checkpoint effect that
        // follows it in the same processing step actually finishes its
        // disk I/O (see `driver::mod`'s doc comment on inline effects) —
        // give it a moment to land before reading the manifest below.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let manifest = load_package_manifest(&nuxd_core::driver::prepare::manifest_path(
            manifests.path(),
            package_id,
        ))
        .unwrap_or_else(|error| panic!("load manifest: {error}"));
        durable_at_pause = manifest.package.files[0].parts[0].durable;
        assert!(
            durable_at_pause > 0,
            "expected some progress to have checkpointed before the pause"
        );
        assert!(
            durable_at_pause < 20_000,
            "expected the pause to have caught it mid-download"
        );

        // Dropped here, without `shutdown()`.
    }

    let manifest = load_package_manifest(&nuxd_core::driver::prepare::manifest_path(
        manifests.path(),
        package_id,
    ))
    .unwrap_or_else(|error| panic!("load manifest: {error}"));
    let queue = QueueManifest {
        schema_version: QUEUE_SCHEMA_VERSION,
        package_order: vec![package_id],
        next_package_id: package_id.get() + 1,
        next_file_id: 100,
        settings: QueueSettings::default(),
    };
    let (state, initial_effects) = EngineState::restore(config, &queue, vec![manifest.package]);
    let driver = Driver::spawn(
        state,
        initial_effects,
        vec![one_link()],
        manifests.path().to_path_buf(),
    );
    let mut events = driver.subscribe();
    driver
        .command(Command::Resume(package_id))
        .await
        .unwrap_or_else(|error| panic!("command: {error}"));

    let done = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match events.recv().await {
                Ok(EngineEvent::PackageDone { .. }) => return true,
                Ok(EngineEvent::PackageFailed { .. }) => return false,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the resumed download never settled"));
    assert!(done, "the resumed download did not complete successfully");

    let bytes =
        std::fs::read(only_file_in(dest.path())).unwrap_or_else(|error| panic!("read: {error}"));
    assert_eq!(bytes, content_range(55, 0, 20_000));

    // Prove it actually resumed rather than restarting from scratch: the
    // resumed attempt's `Range` request must start at or past the
    // checkpointed frontier, not at byte 0.
    let resumed_range = origin
        .requests()
        .iter()
        .rev()
        .find_map(|request| request.range)
        .unwrap_or_else(|| panic!("no ranged request was ever made"));
    assert!(
        resumed_range.start >= durable_at_pause,
        "resumed from byte {} instead of the checkpointed frontier at {durable_at_pause}",
        resumed_range.start
    );

    driver.shutdown().await;
    origin.shutdown().await;
}
