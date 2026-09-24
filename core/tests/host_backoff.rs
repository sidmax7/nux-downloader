//! A busy host (`429`/`503`) is backed off, not failed
//! (`ARCHITECTURE.md` §16). Found with a real server: Hetzner's speed
//! test answers `429` when hit with too many connections, and the engine
//! used to charge each refusal to the block's retries and fail the file.

use nuxd_core::config::EngineConfig;
use nuxd_core::driver::Driver;
use nuxd_core::engine::sim::one_link;
use nuxd_core::engine::{Command, EngineEvent};
use nuxd_core::model::Priority;
use nuxd_testkit::{Fault, Origin, OriginOptions, content_range};
use std::time::Duration;
use tempfile::tempdir;

async fn download(origin: &Origin, dest: &std::path::Path) -> Duration {
    let manifests = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let driver = Driver::spawn_fresh(
        EngineConfig::default(),
        vec![one_link()],
        manifests.path().to_path_buf(),
    );
    let mut events = driver.subscribe();
    let started = std::time::Instant::now();
    driver
        .command(Command::AddPackage {
            name: "pkg".to_string(),
            dest_dir: dest.to_path_buf(),
            sources: vec![origin.url()],
            priority: Priority::Normal,
        })
        .await
        .unwrap_or_else(|e| panic!("AddPackage: {e}"));
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match events.recv().await {
                Ok(EngineEvent::PackageDone { .. }) => return,
                Ok(
                    event @ (EngineEvent::PackageFailed { .. } | EngineEvent::FileFailed { .. }),
                ) => {
                    panic!("download failed: {event:?}")
                }
                Ok(_) => {}
                Err(e) => panic!("events: {e}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("never finished"));
    let elapsed = started.elapsed();
    driver.shutdown().await;
    elapsed
}

fn only_file(dir: &std::path::Path) -> Vec<u8> {
    let files: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir: {e}"))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    assert_eq!(files.len(), 1, "{files:?}");
    std::fs::read(&files[0]).unwrap_or_else(|e| panic!("read: {e}"))
}

#[tokio::test]
async fn a_429_on_a_transfer_waits_then_succeeds() {
    let origin = Origin::start(OriginOptions {
        size: 100_000,
        seed: 21,
        ..Default::default()
    })
    .await;
    // #1 probe, #2 first transfer refused; the retry (#3) is served.
    origin.set_fault(2, Fault::Status(429));
    let dest = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let elapsed = download(&origin, dest.path()).await;
    assert_eq!(only_file(dest.path()), content_range(21, 0, 100_000));
    assert!(
        elapsed >= Duration::from_secs(2),
        "should have backed off: {elapsed:?}"
    );
    origin.shutdown().await;
}

#[tokio::test]
async fn a_429_on_the_probe_waits_then_succeeds() {
    let origin = Origin::start(OriginOptions {
        size: 100_000,
        seed: 22,
        ..Default::default()
    })
    .await;
    origin.set_fault(1, Fault::Status(429));
    let dest = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let elapsed = download(&origin, dest.path()).await;
    assert_eq!(only_file(dest.path()), content_range(22, 0, 100_000));
    assert!(
        elapsed >= Duration::from_secs(2),
        "should have backed off: {elapsed:?}"
    );
    origin.shutdown().await;
}

#[tokio::test]
async fn retry_after_is_honored_on_a_transfer() {
    let origin = Origin::start(OriginOptions {
        size: 100_000,
        seed: 23,
        ..Default::default()
    })
    .await;
    // Our own first backoff would be 2 s; the server asks for 3.
    origin.set_fault(2, Fault::RateLimited(3));
    let dest = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let elapsed = download(&origin, dest.path()).await;
    assert_eq!(only_file(dest.path()), content_range(23, 0, 100_000));
    assert!(
        elapsed >= Duration::from_secs(3),
        "Retry-After ignored: {elapsed:?}"
    );
    origin.shutdown().await;
}

#[tokio::test]
async fn retry_after_is_honored_on_the_probe() {
    let origin = Origin::start(OriginOptions {
        size: 100_000,
        seed: 24,
        ..Default::default()
    })
    .await;
    origin.set_fault(1, Fault::RateLimited(3));
    let dest = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let elapsed = download(&origin, dest.path()).await;
    assert_eq!(only_file(dest.path()), content_range(24, 0, 100_000));
    assert!(
        elapsed >= Duration::from_secs(3),
        "Retry-After ignored: {elapsed:?}"
    );
    origin.shutdown().await;
}
