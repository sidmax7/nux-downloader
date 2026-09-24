//! The probe pool and the collect stage on real sockets
//! (`IMPLEMENTATION_PLAN.md` P4.4): pasting many links never opens more
//! probe connections than configured, whether they go through
//! `Engine::collect` or straight into `Engine::add`.

use nuxd_core::config::EngineConfig;
use nuxd_core::model::Priority;
use nuxd_core::{AddOptions, Engine};
use nuxd_testkit::{Fault, Origin, OriginOptions, RequestedRange, content_range};
use std::time::Duration;
use tempfile::tempdir;

fn is_probe(range: Option<RequestedRange>) -> bool {
    range.is_some_and(|r| r.start == 0 && r.end == Some(0))
}

fn opts(dest: std::path::PathBuf) -> AddOptions {
    AddOptions {
        name: "pkg".to_string(),
        dest_dir: dest,
        priority: Priority::Normal,
    }
}

/// Polls the state (an event can fire before the caller subscribes).
async fn wait_done(engine: &Engine) {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let snapshot = engine.snapshot().await;
            assert!(
                !snapshot
                    .packages
                    .iter()
                    .any(|p| matches!(p.status, nuxd_core::model::PackageStatus::Failed(_))),
                "a package failed"
            );
            if !snapshot.packages.is_empty()
                && snapshot
                    .packages
                    .iter()
                    .all(|p| p.status == nuxd_core::model::PackageStatus::Done)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("never finished"));
}

#[tokio::test]
async fn collect_never_exceeds_the_global_probe_cap() {
    let origin = Origin::start(OriginOptions {
        size: 2000,
        seed: 61,
        ..Default::default()
    })
    .await;
    origin.set_default_fault(Fault::Delay(80));
    let data_dir = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let config = EngineConfig {
        max_concurrent_probes: 4,
        max_probes_per_host: 100,
        ..EngineConfig::default()
    };
    let engine = Engine::start(config, Some(data_dir.path().to_path_buf()))
        .await
        .unwrap_or_else(|e| panic!("start: {e}"));

    let mut inputs: Vec<String> = (0..60).map(|_| origin.url()).collect();
    inputs.push("ftp://example.com/nope.bin".to_string());
    inputs.push("not a url".to_string());
    let drafts = engine.collect(inputs).await;

    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].files.len(), 62, "every input is accounted for");
    assert_eq!(drafts[0].ok_count(), 60);
    assert!(
        drafts[0].files[60]
            .probe
            .as_ref()
            .is_err_and(|e| e.contains("ftp"))
    );
    assert!(drafts[0].files[61].probe.is_err());

    let overlap = origin.max_overlap(|r| is_probe(r.range));
    assert!(
        overlap <= 4,
        "global cap 4, origin saw {overlap} probes at once"
    );
    assert!(
        overlap >= 3,
        "the pool should actually be used, saw {overlap}"
    );
    let probes = origin
        .requests()
        .iter()
        .filter(|r| is_probe(r.range))
        .count();
    assert_eq!(probes, 60, "one probe per resolvable input");

    engine.shutdown().await;
    origin.shutdown().await;
}

#[tokio::test]
async fn collect_caps_probes_per_host() {
    let origin = Origin::start(OriginOptions {
        size: 2000,
        seed: 62,
        ..Default::default()
    })
    .await;
    origin.set_default_fault(Fault::Delay(80));
    let data_dir = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let config = EngineConfig {
        max_concurrent_probes: 32,
        max_probes_per_host: 2,
        ..EngineConfig::default()
    };
    let engine = Engine::start(config, Some(data_dir.path().to_path_buf()))
        .await
        .unwrap_or_else(|e| panic!("start: {e}"));
    let drafts = engine
        .collect((0..20).map(|_| origin.url()).collect())
        .await;
    assert_eq!(drafts[0].ok_count(), 20);
    let overlap = origin.max_overlap(|r| is_probe(r.range));
    assert!(overlap <= 2, "per-host cap 2, origin saw {overlap}");
    engine.shutdown().await;
    origin.shutdown().await;
}

#[tokio::test]
async fn enqueueing_a_draft_downloads_without_probing_again() {
    let size = 3000;
    let origin = Origin::start(OriginOptions {
        size,
        seed: 63,
        ..Default::default()
    })
    .await;
    let data_dir = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let dest = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let engine = Engine::start(EngineConfig::default(), Some(data_dir.path().to_path_buf()))
        .await
        .unwrap_or_else(|e| panic!("start: {e}"));

    let mut drafts = engine.collect(vec![origin.url(), origin.url()]).await;
    let draft = drafts.remove(0);
    engine
        .enqueue(draft, opts(dest.path().to_path_buf()))
        .await
        .unwrap_or_else(|e| panic!("enqueue: {e}"));
    wait_done(&engine).await;

    let probes = origin
        .requests()
        .iter()
        .filter(|r| is_probe(r.range))
        .count();
    assert_eq!(probes, 2, "each file probed once, at collect time only");
    let mut finished = 0;
    for entry in std::fs::read_dir(dest.path()).unwrap_or_else(|e| panic!("read_dir: {e}")) {
        let path = entry.unwrap_or_else(|e| panic!("entry: {e}")).path();
        assert_eq!(
            std::fs::read(&path).unwrap_or_else(|e| panic!("read: {e}")),
            content_range(63, 0, size)
        );
        finished += 1;
    }
    assert_eq!(finished, 2);
    engine.shutdown().await;
    origin.shutdown().await;
}

#[tokio::test]
async fn add_probes_through_the_engines_pool() {
    let origin = Origin::start(OriginOptions {
        size: 1500,
        seed: 64,
        ..Default::default()
    })
    .await;
    origin.set_default_fault(Fault::Delay(60));
    let data_dir = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let dest = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let config = EngineConfig {
        max_concurrent_probes: 3,
        max_probes_per_host: 3,
        ..EngineConfig::default()
    };
    let engine = Engine::start(config, Some(data_dir.path().to_path_buf()))
        .await
        .unwrap_or_else(|e| panic!("start: {e}"));
    engine
        .add(
            (0..30).map(|_| origin.url()).collect(),
            opts(dest.path().to_path_buf()),
        )
        .await
        .unwrap_or_else(|e| panic!("add: {e}"));
    wait_done(&engine).await;

    let overlap = origin.max_overlap(|r| is_probe(r.range));
    assert!(
        overlap <= 3,
        "pool cap 3, origin saw {overlap} probes at once"
    );
    let probes = origin
        .requests()
        .iter()
        .filter(|r| is_probe(r.range))
        .count();
    assert_eq!(probes, 30);
    engine.shutdown().await;
    origin.shutdown().await;
}
