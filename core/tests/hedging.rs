//! Hedging on real sockets (`IMPLEMENTATION_PLAN.md` P7.1): a transfer
//! that crawls is raced by a second attempt once nothing else is left to
//! do, the hedge wins, and the file is byte-exact.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use nuxd_core::config::EngineConfig;
use nuxd_core::engine::sim::one_link;
use nuxd_core::model::{PackageStatus, Priority};
use nuxd_core::net::bind::BindPolicy;
use nuxd_core::net::monitor::LinkSetup;
use nuxd_core::{AddOptions, Engine};
use nuxd_testkit::{Fault, Origin, OriginOptions, content_range};
use std::time::{Duration, Instant};
use tempfile::tempdir;

#[tokio::test]
async fn a_crawling_transfer_is_rescued_by_a_hedge() {
    let size: u64 = 200_000;
    let origin = Origin::start(OriginOptions {
        size,
        seed: 21,
        ..Default::default()
    })
    .await;
    // #1 is the probe, #2 the only transfer: at 2 kB/s it would take
    // 100 s. The hedge (#3) is served at full speed.
    origin.set_fault(2, Fault::Crawl(2_000));

    let data = tempdir().unwrap();
    let dest = tempdir().unwrap();
    let config = EngineConfig {
        hedge_after: Duration::from_secs(1),
        ..Default::default()
    };
    let engine = Engine::start_with(
        config,
        Some(data.path().to_path_buf()),
        LinkSetup::Fixed {
            links: vec![one_link()],
            bind: BindPolicy::Off,
        },
    )
    .await
    .unwrap();
    let start = Instant::now();
    let ids = engine
        .add(
            vec![origin.url()],
            AddOptions {
                name: "p".to_string(),
                dest_dir: dest.path().to_path_buf(),
                priority: Priority::Normal,
                extract: false,
                delete_archives: false,
            },
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let snapshot = engine.snapshot().await;
            if snapshot
                .packages
                .iter()
                .any(|p| p.id == ids[0] && p.status == PackageStatus::Done)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the crawling download was never rescued");
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "{:?}",
        start.elapsed()
    );

    let file = std::fs::read_dir(dest.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    assert_eq!(
        std::fs::read(file.path()).unwrap(),
        content_range(21, 0, size)
    );
    // Probe, the crawling transfer, and exactly one hedge.
    assert_eq!(origin.requests().len(), 3, "{:?}", origin.requests());
    engine.shutdown().await;
}

#[tokio::test]
async fn a_crawling_connection_is_refreshed_on_a_fresh_one() {
    // Several blocks run at once on the one link; the first crawls at
    // 30 kB/s while its siblings fly. After the (shortened) grace
    // periods the crawler is dropped and its block resumes at speed.
    let size: u64 = 64 * 1024 * 1024;
    let origin = Origin::start(OriginOptions {
        size,
        seed: 33,
        ..Default::default()
    })
    .await;
    origin.set_default_fault(Fault::Throttle(2 * 1024 * 1024));
    // #1 is the probe; #2 is the lowest block, started first.
    origin.set_fault(2, Fault::Crawl(30_000));

    let data = tempdir().unwrap();
    let dest = tempdir().unwrap();
    let config = EngineConfig {
        refresh_warmup: Duration::from_millis(300),
        refresh_slow_for: Duration::from_millis(600),
        // Out of the way, so only the refresh can explain the speed.
        hedge_after: Duration::from_secs(60),
        ..Default::default()
    };
    let engine = Engine::start_with(
        config,
        Some(data.path().to_path_buf()),
        LinkSetup::Fixed {
            links: vec![one_link()],
            bind: BindPolicy::Off,
        },
    )
    .await
    .unwrap();
    let start = Instant::now();
    let ids = engine
        .add(
            vec![origin.url()],
            AddOptions {
                name: "p".to_string(),
                dest_dir: dest.path().to_path_buf(),
                priority: Priority::Normal,
                extract: false,
                delete_archives: false,
            },
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let snapshot = engine.snapshot().await;
            if snapshot
                .packages
                .iter()
                .any(|p| p.id == ids[0] && p.status == PackageStatus::Done)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the crawling connection was never refreshed");
    // The crawler alone would need 4 MiB / 30 kB/s ≈ 140 s.
    assert!(
        start.elapsed() < Duration::from_millis(7500),
        "{:?}",
        start.elapsed()
    );

    let file = std::fs::read_dir(dest.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    assert_eq!(
        std::fs::read(file.path()).unwrap(),
        content_range(33, 0, size)
    );
    engine.shutdown().await;
}
