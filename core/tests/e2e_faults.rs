//! The rest of P2.10's M1 integration suite, beyond what
//! `core/tests/driver.rs` already covers (a plain download, and recovery
//! from a cut connection / short body): every remaining testkit fault
//! either recovers or fails as specified (§16), plus the no-range,
//! unknown-size and empty-file cases, and a SHA-256 byte-exact check.

use nuxd_core::config::EngineConfig;
use nuxd_core::driver::Driver;
use nuxd_core::engine::sim::one_link;
use nuxd_core::engine::{Command, EngineEvent};
use nuxd_core::model::Priority;
use nuxd_testkit::{Fault, Origin, OriginOptions, content_range};
use sha2::{Digest, Sha256};
use std::time::Duration;
use tempfile::tempdir;

/// Adds a single-file package and waits (bounded) for the package to
/// settle, returning `true` for `Done`, `false` for `Failed`.
async fn add_and_wait(
    driver: &Driver,
    dest_dir: std::path::PathBuf,
    url: String,
    timeout: Duration,
) -> bool {
    let mut events = driver.subscribe();
    driver
        .command(Command::AddPackage {
            name: "pkg".to_string(),
            dest_dir,
            sources: vec![url],
            priority: Priority::Normal,
        })
        .await
        .unwrap_or_else(|error| panic!("driver: couldn't send AddPackage: {error}"));

    tokio::time::timeout(timeout, async {
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
    .unwrap_or_else(|_| panic!("package never settled within the test timeout"))
}

fn only_file_in(dir: &std::path::Path) -> std::path::PathBuf {
    let files: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("read_dir: {error}"))
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .collect();
    assert_eq!(
        files.len(),
        1,
        "expected exactly one finalized file, found {files:?}"
    );
    files.into_iter().next().unwrap_or_else(|| unreachable!())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

// --- recoveries: a one-shot fault on the first attempt still finishes ---

#[tokio::test]
async fn an_overlong_response_retries_and_recovers() {
    // Multi-block, so the faulted request is a `206` whose `Content-Range`
    // stays truthful and only the body runs long (`TransferError::Overlong`).
    // On a single-block whole-file `200`, the lying `Content-Length` would
    // instead read as the file having changed size (`RemoteChanged`, §10).
    let size = 6 * 1024 * 1024;
    let origin = Origin::start(OriginOptions {
        size,
        seed: 1,
        ..Default::default()
    })
    .await;
    origin.set_fault(2, Fault::Overlong); // request #1 is the probe
    let dest = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let driver = Driver::spawn_fresh(
        EngineConfig::default(),
        vec![one_link()],
        tempdir()
            .unwrap_or_else(|error| panic!("tempdir: {error}"))
            .path()
            .to_path_buf(),
    );

    let done = add_and_wait(
        &driver,
        dest.path().to_path_buf(),
        origin.url(),
        Duration::from_secs(20),
    )
    .await;
    assert!(
        done,
        "an overlong response, retried once, should still finish"
    );
    let bytes =
        std::fs::read(only_file_in(dest.path())).unwrap_or_else(|error| panic!("read: {error}"));
    assert_eq!(bytes, content_range(1, 0, size));

    driver.shutdown().await;
    origin.shutdown().await;
}

// --- permanent failures: a persistent fault exhausts retries as specified ---

#[tokio::test]
async fn a_persistently_wrong_range_start_fails_the_file() {
    // Multi-block (over the 4 MiB threshold) so every block's request
    // carries an explicit `Range` header for `WrongStart` to lie about —
    // a single-block whole-file request never sends one (§8 step 1).
    let size = 6 * 1024 * 1024;
    let origin = Origin::start(OriginOptions {
        size,
        seed: 2,
        ..Default::default()
    })
    .await;
    origin.set_default_fault(Fault::WrongStart);
    let dest = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let config = EngineConfig {
        retry_base_backoff: Duration::from_millis(10),
        retry_max_backoff: Duration::from_millis(50),
        max_retries_per_block: 2,
        ..EngineConfig::default()
    };
    let driver = Driver::spawn_fresh(
        config,
        vec![one_link()],
        tempdir()
            .unwrap_or_else(|error| panic!("tempdir: {error}"))
            .path()
            .to_path_buf(),
    );

    let done = add_and_wait(
        &driver,
        dest.path().to_path_buf(),
        origin.url(),
        Duration::from_secs(15),
    )
    .await;
    assert!(
        !done,
        "a persistently wrong Content-Range must exhaust retries and fail the file"
    );

    driver.shutdown().await;
    origin.shutdown().await;
}

#[tokio::test]
async fn a_persistently_missing_content_range_fails_the_file() {
    let size = 6 * 1024 * 1024;
    let origin = Origin::start(OriginOptions {
        size,
        seed: 3,
        ..Default::default()
    })
    .await;
    origin.set_default_fault(Fault::NoContentRange);
    let dest = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let config = EngineConfig {
        retry_base_backoff: Duration::from_millis(10),
        retry_max_backoff: Duration::from_millis(50),
        max_retries_per_block: 2,
        ..EngineConfig::default()
    };
    let driver = Driver::spawn_fresh(
        config,
        vec![one_link()],
        tempdir()
            .unwrap_or_else(|error| panic!("tempdir: {error}"))
            .path()
            .to_path_buf(),
    );

    let done = add_and_wait(
        &driver,
        dest.path().to_path_buf(),
        origin.url(),
        Duration::from_secs(15),
    )
    .await;
    assert!(
        !done,
        "a persistently missing Content-Range must exhaust retries and fail the file"
    );

    driver.shutdown().await;
    origin.shutdown().await;
}

#[tokio::test]
async fn a_persistent_body_stall_fails_the_file() {
    let origin = Origin::start(OriginOptions {
        size: 50_000,
        seed: 4,
        ..Default::default()
    })
    .await;
    origin.set_default_fault(Fault::StallBody);
    let dest = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let config = EngineConfig {
        stall_timeout: Duration::from_millis(100),
        retry_base_backoff: Duration::from_millis(10),
        retry_max_backoff: Duration::from_millis(50),
        max_retries_per_block: 2,
        ..EngineConfig::default()
    };
    let driver = Driver::spawn_fresh(
        config,
        vec![one_link()],
        tempdir()
            .unwrap_or_else(|error| panic!("tempdir: {error}"))
            .path()
            .to_path_buf(),
    );

    let done = add_and_wait(
        &driver,
        dest.path().to_path_buf(),
        origin.url(),
        Duration::from_secs(10),
    )
    .await;
    assert!(
        !done,
        "a body that never sends another byte must eventually fail the file"
    );

    driver.shutdown().await;
    origin.shutdown().await;
}

#[tokio::test]
async fn an_unreachable_origin_fails_the_probe_and_the_file() {
    // A server error at probe time — `probe::probe` only accepts 200,
    // 206, or a `416 bytes */0` empty file (§5). A `429`/`503` is retried
    // after a host backoff (`host_backoff.rs`), but any other status
    // fails the file at once.
    let origin = Origin::start(OriginOptions {
        size: 1000,
        seed: 5,
        ..Default::default()
    })
    .await;
    origin.set_default_fault(Fault::Status(500));
    let dest = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let driver = Driver::spawn_fresh(
        EngineConfig::default(),
        vec![one_link()],
        tempdir()
            .unwrap_or_else(|error| panic!("tempdir: {error}"))
            .path()
            .to_path_buf(),
    );

    let done = add_and_wait(
        &driver,
        dest.path().to_path_buf(),
        origin.url(),
        Duration::from_secs(10),
    )
    .await;
    assert!(
        !done,
        "a probe that only ever gets a 500 must fail the file"
    );

    driver.shutdown().await;
    origin.shutdown().await;
}

// --- shapes: no-range server, unknown size, empty file ---

#[tokio::test]
async fn a_no_range_server_still_downloads_byte_exact() {
    let origin = Origin::start(OriginOptions {
        size: 10_000,
        seed: 6,
        ranges: false,
        ..Default::default()
    })
    .await;
    let dest = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let driver = Driver::spawn_fresh(
        EngineConfig::default(),
        vec![one_link()],
        tempdir()
            .unwrap_or_else(|error| panic!("tempdir: {error}"))
            .path()
            .to_path_buf(),
    );

    let done = add_and_wait(
        &driver,
        dest.path().to_path_buf(),
        origin.url(),
        Duration::from_secs(10),
    )
    .await;
    assert!(
        done,
        "a no-range server should still complete a plain whole-file GET"
    );
    let bytes =
        std::fs::read(only_file_in(dest.path())).unwrap_or_else(|error| panic!("read: {error}"));
    assert_eq!(bytes, content_range(6, 0, 10_000));

    driver.shutdown().await;
    origin.shutdown().await;
}

#[tokio::test]
async fn an_unknown_size_server_downloads_the_whole_body() {
    let origin = Origin::start(OriginOptions {
        size: 10_000,
        seed: 7,
        content_length: false,
        ..Default::default()
    })
    .await;
    let dest = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let driver = Driver::spawn_fresh(
        EngineConfig::default(),
        vec![one_link()],
        tempdir()
            .unwrap_or_else(|error| panic!("tempdir: {error}"))
            .path()
            .to_path_buf(),
    );

    let done = add_and_wait(
        &driver,
        dest.path().to_path_buf(),
        origin.url(),
        Duration::from_secs(10),
    )
    .await;
    assert!(
        done,
        "an unknown-size (open-ended) download should still complete"
    );
    let bytes =
        std::fs::read(only_file_in(dest.path())).unwrap_or_else(|error| panic!("read: {error}"));
    assert_eq!(bytes, content_range(7, 0, 10_000));

    driver.shutdown().await;
    origin.shutdown().await;
}

#[tokio::test]
async fn an_empty_file_downloads_as_a_zero_byte_file() {
    let origin = Origin::start(OriginOptions {
        size: 0,
        seed: 8,
        ..Default::default()
    })
    .await;
    let dest = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let driver = Driver::spawn_fresh(
        EngineConfig::default(),
        vec![one_link()],
        tempdir()
            .unwrap_or_else(|error| panic!("tempdir: {error}"))
            .path()
            .to_path_buf(),
    );

    let done = add_and_wait(
        &driver,
        dest.path().to_path_buf(),
        origin.url(),
        Duration::from_secs(10),
    )
    .await;
    assert!(
        done,
        "an empty file should complete immediately on activation"
    );
    let bytes =
        std::fs::read(only_file_in(dest.path())).unwrap_or_else(|error| panic!("read: {error}"));
    assert!(bytes.is_empty());

    driver.shutdown().await;
    origin.shutdown().await;
}

// --- a SHA-256 check, alongside the byte-exact comparisons used elsewhere ---

#[tokio::test]
async fn a_downloaded_multi_block_file_hashes_exactly() {
    let size = 5 * 1024 * 1024;
    let origin = Origin::start(OriginOptions {
        size,
        seed: 9,
        ..Default::default()
    })
    .await;
    let dest = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let driver = Driver::spawn_fresh(
        EngineConfig::default(),
        vec![one_link()],
        tempdir()
            .unwrap_or_else(|error| panic!("tempdir: {error}"))
            .path()
            .to_path_buf(),
    );

    let done = add_and_wait(
        &driver,
        dest.path().to_path_buf(),
        origin.url(),
        Duration::from_secs(15),
    )
    .await;
    assert!(done);
    let bytes =
        std::fs::read(only_file_in(dest.path())).unwrap_or_else(|error| panic!("read: {error}"));
    assert_eq!(sha256_hex(&bytes), sha256_hex(&content_range(9, 0, size)));

    driver.shutdown().await;
    origin.shutdown().await;
}
