//! Redirects and expired links on real sockets (`IMPLEMENTATION_PLAN.md`
//! P4.5, `ARCHITECTURE.md` §13): a chain of redirects is followed to the
//! file, a `403`/`410` mid-download re-resolves the source once, and a
//! non-`http` input is rejected.

use nuxd_core::config::EngineConfig;
use nuxd_core::model::Priority;
use nuxd_core::{AddOptions, Engine};
use nuxd_testkit::{Fault, Origin, OriginOptions, RequestedRange, content_range};
use std::time::Duration;
use tempfile::tempdir;

fn is_probe(range: Option<RequestedRange>) -> bool {
    range.is_some_and(|r| r.start == 0 && r.end == Some(0))
}

/// Waits for the queue to settle and reports whether it finished. Polls
/// the state rather than listening for events: a package that fails at
/// once can announce it before the caller has subscribed.
async fn finish(engine: &Engine) -> bool {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let snapshot = engine.snapshot().await;
            if let Some(package) = snapshot.packages.first() {
                match package.status {
                    nuxd_core::model::PackageStatus::Done => return true,
                    nuxd_core::model::PackageStatus::Failed(_) => return false,
                    _ => {}
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("never settled"))
}

fn opts(dest: &std::path::Path) -> AddOptions {
    AddOptions {
        name: "pkg".to_string(),
        dest_dir: dest.to_path_buf(),
        priority: Priority::Normal,
    }
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
async fn a_redirect_chain_is_followed_to_the_file() {
    let size = 5000;
    let target = Origin::start(OriginOptions {
        size,
        seed: 71,
        ..Default::default()
    })
    .await;
    let middle = Origin::start(OriginOptions::default()).await;
    middle.set_default_fault(Fault::Redirect(target.url()));
    let entry = Origin::start(OriginOptions::default()).await;
    entry.set_default_fault(Fault::Redirect(middle.url()));

    let data_dir = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let dest = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let engine = Engine::start(EngineConfig::default(), Some(data_dir.path().to_path_buf()))
        .await
        .unwrap_or_else(|e| panic!("start: {e}"));
    engine
        .add(vec![entry.url()], opts(dest.path()))
        .await
        .unwrap_or_else(|e| panic!("add: {e}"));
    assert!(finish(&engine).await);
    assert_eq!(only_file(dest.path()), content_range(71, 0, size));
    // The bytes came from the end of the chain, not the entry point.
    assert!(target.requests().iter().any(|r| !is_probe(r.range)));
    engine.shutdown().await;
    for origin in [target, middle, entry] {
        origin.shutdown().await;
    }
}

#[tokio::test]
async fn a_redirect_loop_fails_the_file_instead_of_hanging() {
    let origin = Origin::start(OriginOptions::default()).await;
    origin.set_default_fault(Fault::Redirect(origin.url()));
    let data_dir = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let dest = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let engine = Engine::start(EngineConfig::default(), Some(data_dir.path().to_path_buf()))
        .await
        .unwrap_or_else(|e| panic!("start: {e}"));
    engine
        .add(vec![origin.url()], opts(dest.path()))
        .await
        .unwrap_or_else(|e| panic!("add: {e}"));
    assert!(!finish(&engine).await, "a loop must not complete");
    engine.shutdown().await;
    origin.shutdown().await;
}

#[tokio::test]
async fn an_expired_link_is_re_resolved_once_mid_download() {
    for code in [403u16, 410] {
        let size = 50_000;
        let origin = Origin::start(OriginOptions {
            size,
            seed: 72,
            ..Default::default()
        })
        .await;
        // #1 probe, #2 first transfer: the signed URL "expired".
        origin.set_fault(2, Fault::Status(code));
        let data_dir = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let dest = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let engine = Engine::start(EngineConfig::default(), Some(data_dir.path().to_path_buf()))
            .await
            .unwrap_or_else(|e| panic!("start: {e}"));
        engine
            .add(vec![origin.url()], opts(dest.path()))
            .await
            .unwrap_or_else(|e| panic!("add: {e}"));
        assert!(finish(&engine).await, "{code} must be recoverable");
        assert_eq!(only_file(dest.path()), content_range(72, 0, size));

        // probe, refused transfer, re-resolve (a probe), retried transfer.
        let requests = origin.requests();
        assert_eq!(requests.iter().filter(|r| is_probe(r.range)).count(), 2);
        assert_eq!(
            requests.len(),
            4,
            "{:?}",
            requests.iter().map(|r| r.status).collect::<Vec<_>>()
        );
        engine.shutdown().await;
        origin.shutdown().await;
    }
}

#[tokio::test]
async fn a_link_that_stays_forbidden_fails_after_the_one_re_resolve() {
    let origin = Origin::start(OriginOptions {
        size: 50_000,
        seed: 73,
        ..Default::default()
    })
    .await;
    // Every request after the probe is refused.
    for n in 2..=20 {
        origin.set_fault(n, Fault::Status(403));
    }
    let data_dir = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let dest = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let engine = Engine::start(EngineConfig::default(), Some(data_dir.path().to_path_buf()))
        .await
        .unwrap_or_else(|e| panic!("start: {e}"));
    engine
        .add(vec![origin.url()], opts(dest.path()))
        .await
        .unwrap_or_else(|e| panic!("add: {e}"));
    assert!(!finish(&engine).await);
    engine.shutdown().await;
    origin.shutdown().await;
}

#[tokio::test]
async fn a_non_http_input_is_rejected_without_touching_the_network() {
    let data_dir = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let dest = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let engine = Engine::start(EngineConfig::default(), Some(data_dir.path().to_path_buf()))
        .await
        .unwrap_or_else(|e| panic!("start: {e}"));
    engine
        .add(
            vec!["ftp://example.com/a.bin".to_string()],
            opts(dest.path()),
        )
        .await
        .unwrap_or_else(|e| panic!("add: {e}"));
    // Rejected on the spot: the package is already failed (the event may
    // have fired before `add` returned, so read the state instead).
    let snapshot = engine.snapshot().await;
    assert!(matches!(
        snapshot.packages[0].status,
        nuxd_core::model::PackageStatus::Failed(_)
    ));
    let status = &snapshot.packages[0].files[0].status;
    assert!(
        matches!(status, nuxd_core::model::FileStatus::Failed(e) if e.to_string().contains("ftp")),
        "{status:?}"
    );
    engine.shutdown().await;
}
