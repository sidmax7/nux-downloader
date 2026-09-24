//! `Engine`, the public API (`ARCHITECTURE.md` §15, `IMPLEMENTATION_PLAN.md`
//! P2.8's done-when): API-level coverage of every method against the
//! testkit, including a real restart round-trip through `queue.json`.

use nuxd_core::config::EngineConfig;
use nuxd_core::model::Priority;
use nuxd_core::{AddOptions, Engine, Target};
use nuxd_testkit::{Fault, Origin, OriginOptions, content_range};
use std::time::Duration;
use tempfile::tempdir;

fn opts(name: &str, dest_dir: std::path::PathBuf) -> AddOptions {
    AddOptions {
        name: name.to_string(),
        dest_dir,
        priority: Priority::Normal,
    }
}

async fn wait_for_done(engine: &Engine, timeout: Duration) -> bool {
    let mut events = engine.subscribe();
    tokio::time::timeout(timeout, async {
        loop {
            match events.recv().await {
                Ok(nuxd_core::engine::EngineEvent::PackageDone { .. }) => return true,
                Ok(nuxd_core::engine::EngineEvent::PackageFailed { .. }) => return false,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("package never settled"))
}

#[tokio::test]
async fn start_creates_the_data_directory_and_begins_empty() {
    let data_dir = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let data_dir_path = data_dir.path().join("nested").join("nuxd-data");

    let engine = Engine::start(EngineConfig::default(), Some(data_dir_path.clone()))
        .await
        .unwrap_or_else(|error| panic!("start: {error}"));

    assert!(data_dir_path.join("packages").is_dir());
    assert!(engine.snapshot().await.packages.is_empty());

    engine.shutdown().await;
}

#[tokio::test]
async fn add_downloads_a_file_byte_exact() {
    let origin = Origin::start(OriginOptions {
        size: 4000,
        seed: 11,
        ..Default::default()
    })
    .await;
    let data_dir = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let dest = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));

    let engine = Engine::start(EngineConfig::default(), Some(data_dir.path().to_path_buf()))
        .await
        .unwrap_or_else(|error| panic!("start: {error}"));

    let ids = engine
        .add(vec![origin.url()], opts("pkg", dest.path().to_path_buf()))
        .await
        .unwrap_or_else(|error| panic!("add: {error}"));
    assert_eq!(ids.len(), 1);

    assert!(
        wait_for_done(&engine, Duration::from_secs(10)).await,
        "package did not complete"
    );

    let snapshot = engine.snapshot().await;
    assert_eq!(snapshot.packages.len(), 1);
    assert_eq!(snapshot.packages[0].id, ids[0]);

    let files: Vec<_> = std::fs::read_dir(dest.path())
        .unwrap_or_else(|error| panic!("read_dir: {error}"))
        .filter_map(|entry| entry.ok())
        .collect();
    assert_eq!(files.len(), 1);
    let bytes = std::fs::read(files[0].path()).unwrap_or_else(|error| panic!("read: {error}"));
    assert_eq!(bytes, content_range(11, 0, 4000));

    engine.shutdown().await;
}

#[tokio::test]
async fn pause_then_resume_still_completes() {
    let origin = Origin::start(OriginOptions {
        size: 20_000,
        seed: 22,
        ..Default::default()
    })
    .await;
    let data_dir = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let dest = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let engine = Engine::start(EngineConfig::default(), Some(data_dir.path().to_path_buf()))
        .await
        .unwrap_or_else(|error| panic!("start: {error}"));

    let ids = engine
        .add(vec![origin.url()], opts("pkg", dest.path().to_path_buf()))
        .await
        .unwrap_or_else(|error| panic!("add: {error}"));
    let package = ids[0];

    engine
        .pause(Target::Package(package))
        .await
        .unwrap_or_else(|error| panic!("pause: {error}"));
    // Give the pause a moment to actually land before checking status —
    // it's a command sent over a channel, not a synchronous call.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let snapshot = engine.snapshot().await;
    assert_eq!(
        snapshot.packages[0].status,
        nuxd_core::model::PackageStatus::Paused
    );

    engine
        .resume(Target::Package(package))
        .await
        .unwrap_or_else(|error| panic!("resume: {error}"));
    assert!(
        wait_for_done(&engine, Duration::from_secs(10)).await,
        "package did not complete after resume"
    );

    let bytes = std::fs::read(
        std::fs::read_dir(dest.path())
            .unwrap_or_else(|error| panic!("read_dir: {error}"))
            .next()
            .unwrap_or_else(|| panic!("no file"))
            .unwrap_or_else(|error| panic!("entry: {error}"))
            .path(),
    )
    .unwrap_or_else(|error| panic!("read: {error}"));
    assert_eq!(bytes, content_range(22, 0, 20_000));

    engine.shutdown().await;
}

#[tokio::test]
async fn cancel_fails_the_package() {
    let origin = Origin::start(OriginOptions {
        size: 20_000,
        seed: 33,
        ..Default::default()
    })
    .await;
    origin.set_default_fault(Fault::StallHeaders);
    let data_dir = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let dest = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let config = EngineConfig {
        stall_timeout: Duration::from_secs(60),
        ..EngineConfig::default()
    };
    let engine = Engine::start(config, Some(data_dir.path().to_path_buf()))
        .await
        .unwrap_or_else(|error| panic!("start: {error}"));

    // The probe itself stalls forever, so this package never gets past
    // `Unprobed` — cancel must still be able to tear it down.
    let ids = engine
        .add(vec![origin.url()], opts("pkg", dest.path().to_path_buf()))
        .await
        .unwrap_or_else(|error| panic!("add: {error}"));

    engine
        .cancel(Target::Package(ids[0]))
        .await
        .unwrap_or_else(|error| panic!("cancel: {error}"));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let snapshot = engine.snapshot().await;
    assert!(matches!(
        snapshot.packages[0].status,
        nuxd_core::model::PackageStatus::Failed(_)
    ));

    engine.shutdown().await;
}

#[tokio::test]
async fn remove_deletes_the_manifest_and_the_downloaded_file() {
    let origin = Origin::start(OriginOptions {
        size: 4000,
        seed: 44,
        ..Default::default()
    })
    .await;
    let data_dir = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let dest = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let engine = Engine::start(EngineConfig::default(), Some(data_dir.path().to_path_buf()))
        .await
        .unwrap_or_else(|error| panic!("start: {error}"));

    let ids = engine
        .add(vec![origin.url()], opts("pkg", dest.path().to_path_buf()))
        .await
        .unwrap_or_else(|error| panic!("add: {error}"));
    assert!(
        wait_for_done(&engine, Duration::from_secs(10)).await,
        "package did not complete"
    );

    let manifest_path = data_dir
        .path()
        .join("packages")
        .join(format!("{}.json", ids[0]));
    assert!(manifest_path.is_file());

    engine
        .remove(Target::Package(ids[0]), true)
        .await
        .unwrap_or_else(|error| panic!("remove: {error}"));

    assert!(
        !manifest_path.exists(),
        "manifest should be gone after remove"
    );
    assert!(engine.snapshot().await.packages.is_empty());
    let remaining: Vec<_> = std::fs::read_dir(dest.path())
        .unwrap_or_else(|error| panic!("read_dir: {error}"))
        .collect();
    assert!(
        remaining.is_empty(),
        "delete_files should have removed the downloaded file"
    );

    engine.shutdown().await;
}

#[tokio::test]
async fn a_package_survives_a_restart_through_queue_json() {
    let origin = Origin::start(OriginOptions {
        size: 4000,
        seed: 66,
        ..Default::default()
    })
    .await;
    let data_dir = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let dest = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));

    let package_id = {
        let engine = Engine::start(EngineConfig::default(), Some(data_dir.path().to_path_buf()))
            .await
            .unwrap_or_else(|error| panic!("start: {error}"));
        let ids = engine
            .add(vec![origin.url()], opts("pkg", dest.path().to_path_buf()))
            .await
            .unwrap_or_else(|error| panic!("add: {error}"));
        assert!(
            wait_for_done(&engine, Duration::from_secs(10)).await,
            "package did not complete"
        );
        engine.shutdown().await;
        ids[0]
    };

    let engine = Engine::start(EngineConfig::default(), Some(data_dir.path().to_path_buf()))
        .await
        .unwrap_or_else(|error| panic!("restart: {error}"));
    let snapshot = engine.snapshot().await;
    assert_eq!(snapshot.packages.len(), 1);
    assert_eq!(snapshot.packages[0].id, package_id);
    assert_eq!(
        snapshot.packages[0].status,
        nuxd_core::model::PackageStatus::Done
    );

    // A fresh `Engine::start` must not reuse an id `queue.json` already
    // handed out.
    let ids = engine
        .add(vec![origin.url()], opts("pkg2", dest.path().to_path_buf()))
        .await
        .unwrap_or_else(|error| panic!("add: {error}"));
    assert_ne!(ids[0], package_id);

    engine.shutdown().await;
}
