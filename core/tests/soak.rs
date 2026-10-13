//! A long-lived engine under churn (`IMPLEMENTATION_PLAN.md` P8.2): packages
//! come and go while the server flaps, and the process's memory and open
//! file descriptors must stay flat. Complements `cli/tests/chaos.rs`, which
//! kills short-lived processes; this one keeps a single engine alive.
//!
//! Ignored by default; `NUXD_SOAK_SECS` sets the duration (default 60):
//!
//! ```text
//! NUXD_SOAK_SECS=86400 cargo test --release -p nuxd-core --test soak -- --ignored --nocapture
//! ```

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::cast_precision_loss)]

use nuxd_core::config::EngineConfig;
use nuxd_core::engine::sim::one_link;
use nuxd_core::model::{PackageStatus, Priority};
use nuxd_core::net::bind::BindPolicy;
use nuxd_core::net::monitor::LinkSetup;
use nuxd_core::{AddOptions, Engine, Target};
use nuxd_testkit::{Fault, Origin, OriginOptions, content_range};
use std::net::{IpAddr, Ipv4Addr};
use std::time::{Duration, Instant};
use tempfile::tempdir;

fn self_stats() -> (u64, usize) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let rss_kb: u64 = status
        .lines()
        .find(|l| l.starts_with("VmRSS:"))
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    (
        rss_kb / 1024,
        std::fs::read_dir("/proc/self/fd").unwrap().count(),
    )
}

#[tokio::test]
#[ignore = "soak run: see the module docs"]
async fn one_engine_under_churn_stays_flat() {
    let seconds: u64 = std::env::var("NUXD_SOAK_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);
    const SIZE: u64 = 3 * 1024 * 1024;
    let origin = Origin::start(OriginOptions {
        size: SIZE,
        seed: 7,
        ..Default::default()
    })
    .await;
    let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
    origin.shape_source(loopback, 40 * 1024 * 1024, Duration::from_millis(2));

    let data = tempdir().unwrap();
    let engine = Engine::start_with(
        EngineConfig {
            retry_base_backoff: Duration::from_millis(50),
            retry_max_backoff: Duration::from_millis(400),
            ..Default::default()
        },
        Some(data.path().to_path_buf()),
        LinkSetup::Fixed {
            links: vec![one_link()],
            bind: BindPolicy::Off,
        },
    )
    .await
    .unwrap();

    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut samples: Vec<(u64, usize)> = Vec::new();
    let mut last_sample = Instant::now();
    let mut round = 0u64;
    let mut rng = 0x9e37_79b9_7f4a_7c15u64;
    while Instant::now() < deadline {
        round += 1;
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        origin.set_default_fault(match rng % 5 {
            0 => Fault::CutAfter(200_000),
            1 => Fault::Delay(100),
            2 => Fault::EndAfter(300_000),
            _ => Fault::Ok,
        });
        let dest = tempdir().unwrap();
        let ids = engine
            .add(
                (0..4)
                    .map(|i| format!("{}?r={round}&i={i}", origin.url()))
                    .collect(),
                AddOptions {
                    name: format!("round-{round}"),
                    dest_dir: dest.path().to_path_buf(),
                    priority: Priority::Normal,
                    extract: false,
                    delete_archives: false,
                    own_folder: false,
                },
            )
            .await
            .unwrap();
        // Let it run a moment under the weather, then clear it and finish.
        tokio::time::sleep(Duration::from_millis(400)).await;
        origin.block_source(loopback);
        tokio::time::sleep(Duration::from_millis(100)).await;
        origin.unblock_source(loopback);
        origin.set_default_fault(Fault::Ok);
        // Failed files are restarted (weather can fail a probe).
        let finished = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let snapshot = engine.snapshot().await;
                let package = snapshot.packages.iter().find(|p| p.id == ids[0]);
                match package.map(|p| &p.status) {
                    Some(PackageStatus::Done) => return,
                    Some(PackageStatus::Failed(_)) => {
                        for file in &package.unwrap().files {
                            if matches!(file.status, nuxd_core::model::FileStatus::Failed(_)) {
                                let _ = engine.restart_file(file.id).await;
                            }
                        }
                        let _ = engine.resume(Target::Package(ids[0])).await;
                    }
                    _ => {}
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        assert!(finished.is_ok(), "round {round} never finished");
        for entry in std::fs::read_dir(dest.path()).unwrap().flatten() {
            assert_eq!(
                std::fs::read(entry.path()).unwrap(),
                content_range(7, 0, SIZE),
                "round {round}: {:?} is corrupt",
                entry.path()
            );
        }
        // Done packages are removed, as a long-lived app would.
        engine.remove(Target::Package(ids[0]), false).await.unwrap();

        if last_sample.elapsed() >= Duration::from_secs(2) {
            last_sample = Instant::now();
            samples.push(self_stats());
        }
    }
    let quarter = (samples.len() / 4).max(1);
    if samples.len() >= 8 {
        let peak = |part: &[(u64, usize)]| {
            (
                part.iter().map(|s| s.0).max().unwrap(),
                part.iter().map(|s| s.1).max().unwrap(),
            )
        };
        // Skip the first quarter's warm-up: compare the second and the last.
        let (mid_rss, mid_fds) = peak(&samples[quarter..2 * quarter]);
        let (last_rss, last_fds) = peak(&samples[samples.len() - quarter..]);
        eprintln!(
            "soak: {round} rounds; RSS {mid_rss} → {last_rss} MiB, fds {mid_fds} → {last_fds}"
        );
        assert!(last_rss <= mid_rss + mid_rss / 4 + 16, "memory grew");
        assert!(last_fds <= mid_fds + 8, "descriptors grew");
    } else {
        eprintln!("soak: {round} rounds (too few samples to compare)");
    }
    engine.shutdown().await;
}
