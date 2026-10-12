//! A file that changes on the server mid-download (`ARCHITECTURE.md`
//! §10, `IMPLEMENTATION_PLAN.md` P4.6): identical bytes under a new label
//! continue; genuinely different bytes fail only that file, which can
//! then be restarted.

use nuxd_core::config::EngineConfig;
use nuxd_core::engine::sim::one_link;
use nuxd_core::model::{FileStatus, PackageStatus, Priority};
use nuxd_core::net::bind::BindPolicy;
use nuxd_core::net::monitor::LinkSetup;
use nuxd_core::{AddOptions, Engine};
use nuxd_testkit::{Fault, Origin, OriginOptions, content_range};
use std::time::Duration;
use tempfile::tempdir;

const SIZE: u64 = 6 * 1024 * 1024;

/// One slot, so blocks run one after another and a change made after the
/// first block is durable is seen by the next request.
async fn single_slot_engine(data_dir: &std::path::Path) -> Engine {
    let mut link = one_link();
    link.max_slots = 1;
    Engine::start_with(
        EngineConfig::default(),
        Some(data_dir.to_path_buf()),
        LinkSetup::Fixed {
            links: vec![link],
            bind: BindPolicy::Off,
        },
    )
    .await
    .unwrap_or_else(|e| panic!("start: {e}"))
}

fn opts(dest: &std::path::Path) -> AddOptions {
    AddOptions {
        name: "pkg".to_string(),
        dest_dir: dest.to_path_buf(),
        priority: Priority::Normal,
        extract: false,
        delete_archives: false,
    }
}

async fn wait_until<F: Fn(&nuxd_core::engine::EngineSnapshot) -> bool>(
    engine: &Engine,
    what: &str,
    condition: F,
) {
    let ok = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if condition(&engine.snapshot().await) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    if ok.is_err() {
        let snapshot = engine.snapshot().await;
        for package in &snapshot.packages {
            eprintln!("package {:?}", package.status);
            for file in &package.files {
                eprintln!(
                    "  {:?} {:?} durable={:?}",
                    file.name,
                    file.status,
                    file.parts.iter().map(|p| p.durable).collect::<Vec<_>>()
                );
            }
        }
    }
    assert!(ok.is_ok(), "timed out waiting for {what}");
}

fn durable(snapshot: &nuxd_core::engine::EngineSnapshot, file_index: usize) -> u64 {
    snapshot
        .packages
        .first()
        .and_then(|p| p.files.get(file_index))
        .map_or(0, |f| f.parts.iter().map(|part| part.durable).sum())
}

#[tokio::test]
async fn a_relabelled_but_identical_file_continues() {
    let origin = Origin::start(OriginOptions {
        size: SIZE,
        seed: 81,
        ..Default::default()
    })
    .await;
    origin.set_default_fault(Fault::Throttle(8_000_000));
    let data_dir = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let dest = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let engine = single_slot_engine(data_dir.path()).await;
    engine
        .add(vec![origin.url()], opts(dest.path()))
        .await
        .unwrap_or_else(|e| panic!("add: {e}"));

    wait_until(&engine, "some bytes to be durable", |s| durable(s, 0) > 0).await;
    // A load balancer starts labelling the same bytes differently.
    origin.set_etag(Some("\"relabelled\""));

    wait_until(&engine, "the package to finish", |s| {
        s.packages
            .first()
            .is_some_and(|p| p.status == PackageStatus::Done)
    })
    .await;
    let bytes = std::fs::read(
        std::fs::read_dir(dest.path())
            .unwrap_or_else(|e| panic!("read_dir: {e}"))
            .next()
            .unwrap_or_else(|| panic!("no file"))
            .unwrap_or_else(|e| panic!("entry: {e}"))
            .path(),
    )
    .unwrap_or_else(|e| panic!("read: {e}"));
    assert_eq!(bytes, content_range(81, 0, SIZE));

    // The engine really did settle it by sampling: small ranged requests.
    let samples = origin
        .requests()
        .iter()
        .filter(|r| {
            r.range
                .is_some_and(|r| r.end.is_some_and(|end| end - r.start + 1 == 16 * 1024))
        })
        .count();
    assert!(samples >= 1, "expected 16 KiB sample requests");
    engine.shutdown().await;
    origin.shutdown().await;
}

#[tokio::test]
async fn a_genuinely_changed_file_fails_alone_and_can_be_restarted() {
    let changing = Origin::start(OriginOptions {
        size: SIZE,
        seed: 82,
        ..Default::default()
    })
    .await;
    changing.set_default_fault(Fault::Throttle(8_000_000));
    let stable = Origin::start(OriginOptions {
        size: 400_000,
        seed: 83,
        ..Default::default()
    })
    .await;
    let data_dir = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let dest = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let engine = single_slot_engine(data_dir.path()).await;
    engine
        .add(vec![changing.url(), stable.url()], opts(dest.path()))
        .await
        .unwrap_or_else(|e| panic!("add: {e}"));

    wait_until(&engine, "the first file to start", |s| durable(s, 0) > 0).await;
    // Republished: same size, different bytes, new label.
    changing.set_seed(999);
    changing.set_etag(Some("\"v2\""));

    wait_until(
        &engine,
        "the changed file to fail and the other to finish",
        |s| {
            s.packages.first().is_some_and(|p| {
                matches!(p.files[0].status, FileStatus::Failed(_))
                    && p.files[1].status == FileStatus::Verified
            })
        },
    )
    .await;
    let snapshot = engine.snapshot().await;
    let package = &snapshot.packages[0];
    assert!(matches!(
        &package.files[0].status,
        FileStatus::Failed(nuxd_core::model::FileError::RemoteChanged { .. })
    ));
    assert_eq!(
        package.status,
        PackageStatus::Failed(nuxd_core::model::PackageError::FilesFailed)
    );
    // The stable file was not affected.
    assert_eq!(
        std::fs::read(dest.path().join(&package.files[1].name))
            .unwrap_or_else(|e| panic!("read stable: {e}")),
        content_range(83, 0, 400_000)
    );

    // The file is republished again (back to the original); restart it.
    changing.set_seed(82);
    changing.set_etag(Some("\"testkit-etag\""));
    changing.set_default_fault(Fault::Ok);
    let failed_id = package.files[0].id;
    engine
        .restart_file(failed_id)
        .await
        .unwrap_or_else(|e| panic!("restart: {e}"));
    wait_until(&engine, "the restarted file to complete", |s| {
        s.packages
            .first()
            .is_some_and(|p| p.status == PackageStatus::Done)
    })
    .await;
    let snapshot = engine.snapshot().await;
    let name = snapshot.packages[0].files[0].name.clone();
    assert_eq!(
        std::fs::read(dest.path().join(&name)).unwrap_or_else(|e| panic!("read: {e}")),
        content_range(82, 0, SIZE),
        "restarted from scratch, byte-exact"
    );
    // No stale partial data or extra placeholder was left behind.
    let leftovers: Vec<_> = std::fs::read_dir(dest.path())
        .unwrap_or_else(|e| panic!("read_dir: {e}"))
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(leftovers.len(), 2, "{leftovers:?}");
    engine.shutdown().await;
    changing.shutdown().await;
    stable.shutdown().await;
}
