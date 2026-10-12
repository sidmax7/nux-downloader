//! Speed limits through the whole engine on real sockets
//! (`IMPLEMENTATION_PLAN.md` P6.1): global, per package and per link, set
//! before and during a download. The exact accuracy is tested with paused
//! time in `limit.rs`; here the point is that transfers really wait.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use nuxd_core::config::EngineConfig;
use nuxd_core::engine::sim::one_link;
use nuxd_core::model::{ByteRate, PackageStatus, Priority};
use nuxd_core::net::bind::BindPolicy;
use nuxd_core::net::monitor::LinkSetup;
use nuxd_core::{AddOptions, Engine};
use nuxd_testkit::{Origin, OriginOptions, content_range};
use std::path::Path;
use std::time::{Duration, Instant};
use tempfile::tempdir;

fn rate(n: u64) -> Option<ByteRate> {
    ByteRate::new(n)
}

async fn engine(data_dir: &Path, config: EngineConfig) -> Engine {
    Engine::start_with(
        config,
        Some(data_dir.to_path_buf()),
        LinkSetup::Fixed {
            links: vec![one_link()],
            bind: BindPolicy::Off,
        },
    )
    .await
    .unwrap()
}

fn opts(name: &str, dest: &Path) -> AddOptions {
    AddOptions {
        name: name.to_string(),
        dest_dir: dest.to_path_buf(),
        priority: Priority::Normal,
        extract: false,
        delete_archives: false,
    }
}

async fn done(engine: &Engine, package: nuxd_core::model::PackageId, within: Duration) -> Duration {
    let start = Instant::now();
    tokio::time::timeout(within, async {
        loop {
            let snapshot = engine.snapshot().await;
            if snapshot
                .packages
                .iter()
                .any(|p| p.id == package && p.status == PackageStatus::Done)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the download never finished");
    start.elapsed()
}

#[tokio::test]
async fn a_global_limit_slows_a_download_to_about_that_speed() {
    let size = 400_000;
    let origin = Origin::start(OriginOptions {
        size,
        seed: 3,
        ..Default::default()
    })
    .await;
    let data = tempdir().unwrap();
    let dest = tempdir().unwrap();
    let config = EngineConfig {
        global_limit: rate(200_000),
        ..Default::default()
    };
    let engine = engine(data.path(), config).await;
    let ids = engine
        .add(vec![origin.url()], opts("p", dest.path()))
        .await
        .unwrap();
    let took = done(&engine, ids[0], Duration::from_secs(30)).await;
    // 400 kB at 200 kB/s is about 2 s (minus the burst).
    assert!(
        took > Duration::from_millis(1500) && took < Duration::from_millis(3500),
        "took {took:?}"
    );
    let file = std::fs::read_dir(dest.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    assert_eq!(
        std::fs::read(file.path()).unwrap(),
        content_range(3, 0, size)
    );
    engine.shutdown().await;
}

#[tokio::test]
async fn lifting_the_limit_during_a_download_speeds_it_up() {
    let size = 2_000_000;
    let origin = Origin::start(OriginOptions {
        size,
        seed: 4,
        ..Default::default()
    })
    .await;
    let data = tempdir().unwrap();
    let dest = tempdir().unwrap();
    let config = EngineConfig {
        global_limit: rate(100_000),
        ..Default::default()
    };
    let engine = engine(data.path(), config).await;
    let ids = engine
        .add(vec![origin.url()], opts("p", dest.path()))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(engine.global_limit(), rate(100_000));
    engine.set_global_limit(None);
    assert_eq!(engine.global_limit(), None);
    let took = done(&engine, ids[0], Duration::from_secs(15)).await;
    assert!(took < Duration::from_secs(8), "still slow: {took:?}");
    engine.shutdown().await;
}

#[tokio::test]
async fn a_package_limit_holds_back_only_that_package() {
    let slow_origin = Origin::start(OriginOptions {
        size: 300_000,
        seed: 5,
        ..Default::default()
    })
    .await;
    let fast_origin = Origin::start(OriginOptions {
        size: 300_000,
        seed: 6,
        ..Default::default()
    })
    .await;
    // Nothing of the slow package's first transfer (request 2; 1 is the
    // probe) gets through before the pause, so all of it runs limited.
    slow_origin.hold_request_at(2, 0);
    let data = tempdir().unwrap();
    let dest = tempdir().unwrap();
    let engine = engine(data.path(), EngineConfig::default()).await;
    let slow = engine
        .add(vec![slow_origin.url()], opts("slow", dest.path()))
        .await
        .unwrap()[0];
    // Pause while the limit is set, so nothing downloads unlimited first.
    engine
        .pause(nuxd_core::Target::Package(slow))
        .await
        .unwrap();
    engine.set_package_limit(slow, rate(100_000)).await.unwrap();
    engine
        .resume(nuxd_core::Target::Package(slow))
        .await
        .unwrap();
    let fast = engine
        .add(vec![fast_origin.url()], opts("fast", dest.path()))
        .await
        .unwrap()[0];

    let fast_took = done(&engine, fast, Duration::from_secs(20)).await;
    assert!(fast_took < Duration::from_millis(1500), "{fast_took:?}");
    let slow_took = done(&engine, slow, Duration::from_secs(20)).await;
    // 300 kB at 100 kB/s: about 3 s from the start, so the remainder is
    // still well over a second after the fast one finished.
    assert!(
        fast_took + slow_took > Duration::from_millis(2200),
        "slow package wasn't limited: {fast_took:?} + {slow_took:?}"
    );

    // The limit is part of the saved package.
    let snapshot = engine.snapshot().await;
    assert_eq!(
        snapshot
            .packages
            .iter()
            .find(|p| p.id == slow)
            .unwrap()
            .limit,
        rate(100_000)
    );
    engine.shutdown().await;
}

#[tokio::test]
async fn a_link_limit_applies_to_what_runs_on_that_link() {
    let origin = Origin::start(OriginOptions {
        size: 300_000,
        seed: 7,
        ..Default::default()
    })
    .await;
    let data = tempdir().unwrap();
    let dest = tempdir().unwrap();
    let engine = engine(data.path(), EngineConfig::default()).await;
    let link = one_link().id;
    engine.set_link_limit(link, rate(100_000));
    assert_eq!(engine.link_limit(link), rate(100_000));
    let id = engine
        .add(vec![origin.url()], opts("p", dest.path()))
        .await
        .unwrap()[0];
    let took = done(&engine, id, Duration::from_secs(20)).await;
    assert!(took > Duration::from_millis(2000), "{took:?}");
    engine.shutdown().await;
}

#[tokio::test]
async fn a_package_limit_survives_a_restart() {
    let origin = Origin::start(OriginOptions {
        size: 300_000,
        seed: 8,
        ..Default::default()
    })
    .await;
    let data = tempdir().unwrap();
    let dest = tempdir().unwrap();
    // The first transfer (request 2; 1 is the probe) sends nothing until
    // the pause lands, so the whole file is left for the limited run. On
    // a fast machine it could otherwise finish between `add` and `pause`.
    origin.hold_request_at(2, 0);
    {
        let engine = engine(data.path(), EngineConfig::default()).await;
        // Pause first so nothing downloads before we shut down.
        let id = engine
            .add(vec![origin.url()], opts("p", dest.path()))
            .await
            .unwrap()[0];
        engine.pause(nuxd_core::Target::Package(id)).await.unwrap();
        engine.set_package_limit(id, rate(100_000)).await.unwrap();
        engine.shutdown().await;
    }
    let engine = engine(data.path(), EngineConfig::default()).await;
    let snapshot = engine.snapshot().await;
    assert_eq!(snapshot.packages[0].limit, rate(100_000));
    let id = snapshot.packages[0].id;
    engine.resume(nuxd_core::Target::All).await.unwrap();
    let took = done(&engine, id, Duration::from_secs(20)).await;
    assert!(took > Duration::from_millis(1800), "{took:?}");
    engine.shutdown().await;
}
