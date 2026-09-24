//! Lazy activation on real sockets (`IMPLEMENTATION_PLAN.md` P4.2): many
//! files through few slots all complete byte-exact, and only about as
//! many placeholders exist at once as the slots can use.

use nuxd_core::config::EngineConfig;
use nuxd_core::driver::Driver;
use nuxd_core::engine::sim::one_link;
use nuxd_core::engine::{Command, EngineEvent};
use nuxd_core::model::Priority;
use nuxd_testkit::{Fault, Origin, OriginOptions, content_range};
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
    // Slow enough that files overlap, so the slot limit matters.
    origin.set_default_fault(Fault::Throttle(400_000));

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
        })
        .await
        .unwrap_or_else(|e| panic!("AddPackage: {e}"));

    // While it runs, count how many `.nuxdpart` placeholders exist at
    // once: activation is bounded by the free slots (3).
    let mut most_placeholders = 0;
    let watch = async {
        loop {
            let parts = std::fs::read_dir(dest.path())
                .map(|entries| {
                    entries
                        .filter_map(Result::ok)
                        .filter(|e| e.path().extension().is_some_and(|x| x == "nuxdpart"))
                        .count()
                })
                .unwrap_or(0);
            most_placeholders = most_placeholders.max(parts);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
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
    tokio::select! {
        () = watch => {}
        result = tokio::time::timeout(Duration::from_secs(60), done) => {
            assert!(result.is_ok(), "never finished");
        }
    }

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
    // 3 slots' worth, plus files whose rename is still queued on the
    // durability lane — nowhere near all 20.
    assert!(
        most_placeholders <= 6,
        "activation should track the slots (3), saw {most_placeholders} placeholders at once"
    );
    driver.shutdown().await;
    origin.shutdown().await;
}
