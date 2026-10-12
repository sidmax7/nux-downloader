//! Lazy activation on real sockets (`IMPLEMENTATION_PLAN.md` P4.2): many
//! files through few slots all complete byte-exact, and only about as
//! many placeholders exist at once as the slots can use.

use nuxd_core::config::EngineConfig;
use nuxd_core::driver::Driver;
use nuxd_core::engine::sim::one_link;
use nuxd_core::engine::{Command, EngineEvent};
use nuxd_core::model::Priority;
use nuxd_testkit::{Origin, OriginOptions, content_range};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tempfile::tempdir;

#[tokio::test]
async fn twenty_files_through_three_slots_all_complete() {
    let size = 20_000;
    let origin = Origin::start(OriginOptions {
        size,
        seed: 41,
        ..Default::default()
    })
    .await;
    // Every transfer stops halfway until released (a probe is one byte, so
    // it isn't held). With nothing finished, the placeholders on disk are
    // exactly the files activated so far. Counted while files were
    // finishing, they also included files whose rename was still queued
    // behind `fsync`s on the durability lane, and how many that is depends
    // on the disk (8 or 9 on the macOS and Windows CI runners), not on
    // activation.
    let halfway = size / 2;
    origin.hold_every_request_at(halfway);

    let dest = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let manifests = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let mut link = one_link();
    link.max_slots = 3;
    let driver = Driver::spawn_fresh(
        EngineConfig::default(),
        vec![link],
        manifests.path().to_path_buf(),
    );
    let mut events = driver.subscribe();
    driver
        .command(Command::AddPackage {
            name: "many".to_string(),
            dest_dir: dest.path().to_path_buf(),
            sources: (0..20).map(|_| origin.url()).collect(),
            priority: Priority::Normal,
            post: Default::default(),
        })
        .await
        .unwrap_or_else(|e| panic!("AddPackage: {e}"));

    // The three slots fill with held transfers; then nothing else may be
    // activated, however long they stay held (well under `hedge_after`).
    let held = || {
        origin
            .requests()
            .iter()
            .filter(|r| r.bytes_sent.load(Ordering::Relaxed) == halfway)
            .count()
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        while held() < 3 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("three transfers never started, {} held", held()));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(held(), 3, "only the three slots' transfers should run");
    assert_eq!(
        placeholders(dest.path()),
        3,
        "activation should track the slots (3)"
    );
    origin.release();

    let done = async {
        loop {
            match events.recv().await {
                Ok(EngineEvent::PackageDone { .. }) => return,
                Ok(
                    event @ (EngineEvent::PackageFailed { .. } | EngineEvent::FileFailed { .. }),
                ) => {
                    panic!("failed: {event:?}")
                }
                Ok(_) => {}
                Err(e) => panic!("events: {e}"),
            }
        }
    };
    assert!(
        tokio::time::timeout(Duration::from_secs(60), done)
            .await
            .is_ok(),
        "never finished"
    );

    let expected = content_range(41, 0, size);
    let files: Vec<_> = std::fs::read_dir(dest.path())
        .unwrap_or_else(|e| panic!("read_dir: {e}"))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    assert_eq!(
        files.len(),
        20,
        "20 finished files, no leftovers: {files:?}"
    );
    for file in files {
        assert_eq!(
            std::fs::read(&file).unwrap_or_else(|e| panic!("read: {e}")),
            expected
        );
    }
    driver.shutdown().await;
    origin.shutdown().await;
}

/// Decision 30: a finished file keeps its `.nuxdpart` (and an open handle)
/// until its rename has run on the durability lane, so activation waits
/// while `max_pending_finalizes` of them are queued. With a limit of 1 the
/// run only finishes if the driver reports every rename back, and a
/// placeholder on disk is either an active file (at most one per slot) or
/// the one waiting rename: never more than 3 + 1, however fast or slow
/// the disk is.
#[tokio::test]
async fn files_waiting_for_their_rename_hold_back_activation() {
    let size = 20_000;
    let origin = Origin::start(OriginOptions {
        size,
        seed: 42,
        ..Default::default()
    })
    .await;
    let dest = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let manifests = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let mut link = one_link();
    link.max_slots = 3;
    let config = EngineConfig {
        max_pending_finalizes: 1,
        ..EngineConfig::default()
    };
    let driver = Driver::spawn_fresh(config, vec![link], manifests.path().to_path_buf());
    let mut events = driver.subscribe();
    driver
        .command(Command::AddPackage {
            name: "many".to_string(),
            dest_dir: dest.path().to_path_buf(),
            sources: (0..20).map(|_| origin.url()).collect(),
            priority: Priority::Normal,
            post: Default::default(),
        })
        .await
        .unwrap_or_else(|e| panic!("AddPackage: {e}"));

    let mut most = 0;
    let watch = async {
        loop {
            most = most.max(placeholders(dest.path()));
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    };
    let done = async {
        loop {
            match events.recv().await {
                Ok(EngineEvent::PackageDone { .. }) => return,
                Ok(
                    event @ (EngineEvent::PackageFailed { .. } | EngineEvent::FileFailed { .. }),
                ) => panic!("failed: {event:?}"),
                Ok(_) => {}
                Err(e) => panic!("events: {e}"),
            }
        }
    };
    tokio::select! {
        () = watch => {}
        result = tokio::time::timeout(Duration::from_secs(30), done) => {
            assert!(result.is_ok(), "stalled: a rename was never reported back");
        }
    }
    assert!(
        most <= 4,
        "{most} placeholders at once, at most 3 + 1 expected"
    );

    let expected = content_range(42, 0, size);
    let files: Vec<_> = std::fs::read_dir(dest.path())
        .unwrap_or_else(|e| panic!("read_dir: {e}"))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    assert_eq!(files.len(), 20, "{files:?}");
    for file in files {
        assert_eq!(
            std::fs::read(&file).unwrap_or_else(|e| panic!("read: {e}")),
            expected
        );
    }
    driver.shutdown().await;
    origin.shutdown().await;
}

/// How many `.nuxdpart` files are in `dir`.
fn placeholders(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|e| e.path().extension().is_some_and(|x| x == "nuxdpart"))
                .count()
        })
        .unwrap_or(0)
}
