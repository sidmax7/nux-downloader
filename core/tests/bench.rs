//! The benchmark harness (`IMPLEMENTATION_PLAN.md` P7.3). Real engine, real
//! sockets, shaped loopback "links": each link is a loopback alias
//! (`127.0.0.N`) whose connections share one rate and a fixed latency
//! (`Origin::shape_source`). Results are printed as Markdown rows; they feed
//! `docs/benchmarks.md` and the defaults in `ARCHITECTURE.md` §19.
//!
//! It is `#[ignore]`d — it takes minutes and measures wall-clock time:
//!
//! ```text
//! cargo test --release -p nuxd-core --test bench -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Two numbers per run besides completion time:
//! - **efficiency**: the ideal time (bytes ÷ the links' summed rates) over
//!   the measured one;
//! - **idle slot-seconds**: over the window from the first request to the
//!   last response, the connection slots the links offered minus the time
//!   requests were actually in flight.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::cast_precision_loss)]

use nuxd_core::config::EngineConfig;
use nuxd_core::driver::Driver;
use nuxd_core::engine::{Command, EngineEvent, EngineState};
use nuxd_core::model::{Link, LinkId, LinkKind, Priority};
use nuxd_core::net::bind::BindPolicy;
use nuxd_core::net::monitor::LinkSetup;
use nuxd_testkit::{Fault, Origin, OriginOptions};
use std::net::{IpAddr, Ipv4Addr};
use std::time::{Duration, Instant};
use tempfile::tempdir;

const MIB: u64 = 1024 * 1024;

#[derive(Clone, Copy)]
struct LinkSpec {
    /// Last octet of the loopback alias.
    id: u8,
    /// Shared rate of the whole link, bytes/s.
    rate: u64,
    latency_ms: u64,
}

impl LinkSpec {
    fn ip(self) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, self.id))
    }
}

struct Outcome {
    secs: f64,
    ideal_secs: f64,
    idle_slot_secs: f64,
    requests: usize,
}

fn link(spec: LinkSpec, slots: u8) -> Link {
    Link {
        id: LinkId::new(u64::from(spec.id)),
        name: format!("lo{}", spec.id),
        label: format!("loopback .{}", spec.id),
        kind: LinkKind::Other,
        v4: Some(Ipv4Addr::new(127, 0, 0, spec.id)),
        v6: None,
        max_slots: slots,
        limit: None,
    }
}

/// Downloads `files` copies of an `size`-byte file over `links` and
/// measures it. `tweak` can arm per-request faults on the origin.
async fn run(
    links: &[LinkSpec],
    slots: u8,
    config: EngineConfig,
    size: u64,
    files: usize,
    tweak: impl FnOnce(&Origin),
) -> Outcome {
    let origin = Origin::start(OriginOptions {
        size,
        seed: 1,
        ..Default::default()
    })
    .await;
    for spec in links {
        origin.shape_source(spec.ip(), spec.rate, Duration::from_millis(spec.latency_ms));
    }
    tweak(&origin);

    let data = tempdir().unwrap();
    let dest = tempdir().unwrap();
    let driver = Driver::spawn_with(
        EngineState::new(config),
        Vec::new(),
        LinkSetup::Fixed {
            links: links.iter().map(|&spec| link(spec, slots)).collect(),
            bind: BindPolicy::Address,
        },
        data.path().to_path_buf(),
    );
    let mut events = driver.subscribe();
    let started = Instant::now();
    driver
        .command(Command::AddPackage {
            name: "bench".to_string(),
            dest_dir: dest.path().to_path_buf(),
            sources: (0..files).map(|_| origin.url()).collect(),
            priority: Priority::Normal,
            post: Default::default(),
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(300), async {
        loop {
            match events.recv().await {
                Ok(EngineEvent::PackageDone { .. }) => return,
                Ok(e @ (EngineEvent::PackageFailed { .. } | EngineEvent::FileFailed { .. })) => {
                    panic!("download failed: {e:?}")
                }
                Ok(_) => {}
                Err(e) => panic!("events: {e}"),
            }
        }
    })
    .await
    .expect("benchmark run never finished");
    let secs = started.elapsed().as_secs_f64();

    // Slot accounting from the origin's own request log.
    let requests = origin.requests();
    let now = Instant::now();
    let first = requests.iter().map(|r| r.started).min().unwrap_or(started);
    let last = requests
        .iter()
        .map(|r| r.finished.lock().ok().and_then(|f| *f).unwrap_or(now))
        .max()
        .unwrap_or(now);
    let window = last.duration_since(first).as_secs_f64();
    let busy: f64 = requests
        .iter()
        .map(|r| {
            let end = r.finished.lock().ok().and_then(|f| *f).unwrap_or(now);
            end.duration_since(r.started).as_secs_f64()
        })
        .sum();
    let capacity = window * f64::from(slots) * links.len() as f64;

    let total_rate: u64 = links.iter().map(|l| l.rate).sum();
    let ideal_secs = (size * files as u64) as f64 / total_rate as f64;
    let outcome = Outcome {
        secs,
        ideal_secs,
        idle_slot_secs: (capacity - busy).max(0.0),
        requests: requests.len(),
    };
    driver.shutdown().await;
    origin.shutdown().await;
    outcome
}

fn row(label: &str, o: &Outcome) {
    println!(
        "| {label} | {:.2} s | {:.2} s | {:.0}% | {:.1} | {} |",
        o.secs,
        o.ideal_secs,
        100.0 * o.ideal_secs / o.secs,
        o.idle_slot_secs,
        o.requests
    );
}

fn header(title: &str) {
    println!("\n### {title}\n");
    println!("| Run | Time | Ideal | Efficiency | Idle slot-s | Requests |");
    println!("|---|---|---|---|---|---|");
}

const FAST: LinkSpec = LinkSpec {
    id: 2,
    rate: 40 * MIB,
    latency_ms: 20,
};
const SLOW: LinkSpec = LinkSpec {
    id: 3,
    rate: 10 * MIB,
    latency_ms: 60,
};

#[tokio::test]
#[ignore = "benchmark: run explicitly (see the module docs)"]
async fn aggregation() {
    header("One 128 MiB file: one link versus two (40 MiB/s @ 20 ms, 10 MiB/s @ 60 ms)");
    let config = EngineConfig::default();
    for (label, links) in [
        ("fast only", vec![FAST]),
        ("slow only", vec![SLOW]),
        ("fast + slow", vec![FAST, SLOW]),
    ] {
        let o = run(&links, 8, config.clone(), 128 * MIB, 1, |_| {}).await;
        row(label, &o);
    }
}

#[tokio::test]
#[ignore = "benchmark: run explicitly (see the module docs)"]
async fn many_small_files_and_slots() {
    header("200 × 100 KiB files on the fast link: slots per link (1 = sequential)");
    for slots in [1u8, 2, 4, 8, 16] {
        let config = EngineConfig {
            slots_per_link: slots,
            ..Default::default()
        };
        let o = run(&[FAST], slots, config, 100 * 1024, 200, |_| {}).await;
        row(&format!("{slots} slot(s)"), &o);
    }
}

#[tokio::test]
#[ignore = "benchmark: run explicitly (see the module docs)"]
async fn block_size() {
    header("One 256 MiB file on the fast link: block size");
    for mib in [1u64, 2, 4, 8, 16] {
        let config = EngineConfig {
            min_block_bytes: mib * MIB,
            max_block_bytes: mib * MIB,
            ..Default::default()
        };
        let o = run(&[FAST], 8, config, 256 * MIB, 1, |_| {}).await;
        row(&format!("{mib} MiB blocks"), &o);
    }
}

#[tokio::test]
#[ignore = "benchmark: run explicitly (see the module docs)"]
async fn tail_handling() {
    header("One 32 MiB file, one block served at 100 kB/s: tail handling");
    let slow_block = |origin: &Origin| origin.set_fault(2, Fault::Throttle(100_000));
    let base = || EngineConfig {
        min_block_bytes: MIB,
        max_block_bytes: MIB,
        ..Default::default()
    };
    let off = EngineConfig {
        max_hedges_per_block: 0,
        max_refreshes_per_block: 0,
        ..base()
    };
    row(
        "neither",
        &run(&[FAST], 8, off, 32 * MIB, 1, slow_block).await,
    );

    let refresh_only = EngineConfig {
        max_hedges_per_block: 0,
        ..base()
    };
    row(
        "refresh only (3 s warm-up + 5 s)",
        &run(&[FAST], 8, refresh_only, 32 * MIB, 1, slow_block).await,
    );

    for secs in [1u64, 2, 4, 8] {
        let hedge_only = EngineConfig {
            max_refreshes_per_block: 0,
            hedge_after: Duration::from_secs(secs),
            ..base()
        };
        row(
            &format!("hedge only, after {secs} s"),
            &run(&[FAST], 8, hedge_only, 32 * MIB, 1, slow_block).await,
        );
    }
    row(
        "both (defaults)",
        &run(&[FAST], 8, base(), 32 * MIB, 1, slow_block).await,
    );
}

#[tokio::test]
#[ignore = "benchmark: run explicitly (see the module docs)"]
async fn hedge_after_with_two_links() {
    header("One 128 MiB file over fast + slow links: hedge_after");
    for secs in [1u64, 2, 4, 8] {
        let config = EngineConfig {
            hedge_after: Duration::from_secs(secs),
            ..Default::default()
        };
        let o = run(&[FAST, SLOW], 8, config, 128 * MIB, 1, |_| {}).await;
        row(&format!("hedge after {secs} s"), &o);
    }
    let off = EngineConfig {
        max_hedges_per_block: 0,
        ..Default::default()
    };
    row(
        "no hedging",
        &run(&[FAST, SLOW], 8, off, 128 * MIB, 1, |_| {}).await,
    );
}

#[tokio::test]
#[ignore = "benchmark: run explicitly (see the module docs)"]
async fn many_files_with_a_slow_one() {
    header("20 × 8 MiB files over fast + slow links: hedge_after");
    for secs in [1u64, 2, 8] {
        let config = EngineConfig {
            hedge_after: Duration::from_secs(secs),
            ..Default::default()
        };
        let o = run(&[FAST, SLOW], 8, config, 8 * MIB, 20, |_| {}).await;
        row(&format!("hedge after {secs} s"), &o);
    }
}

#[tokio::test]
#[ignore = "benchmark: run explicitly (see the module docs)"]
async fn refresh_with_peers() {
    header("One 512 MiB file, one 8 MiB block served at 200 kB/s: refresh versus hedge");
    let slow_block = |origin: &Origin| origin.set_fault(2, Fault::Throttle(200_000));
    let neither = EngineConfig {
        max_hedges_per_block: 0,
        max_refreshes_per_block: 0,
        ..Default::default()
    };
    row(
        "neither",
        &run(&[FAST], 8, neither, 512 * MIB, 1, slow_block).await,
    );
    for (warmup, slow_for) in [(3u64, 5u64), (2, 3), (1, 2)] {
        let refresh = EngineConfig {
            max_hedges_per_block: 0,
            refresh_warmup: Duration::from_secs(warmup),
            refresh_slow_for: Duration::from_secs(slow_for),
            ..Default::default()
        };
        row(
            &format!("refresh only ({warmup} s warm-up + {slow_for} s)"),
            &run(&[FAST], 8, refresh, 512 * MIB, 1, slow_block).await,
        );
    }
    let hedge = EngineConfig {
        max_refreshes_per_block: 0,
        ..Default::default()
    };
    row(
        "hedge only (default)",
        &run(&[FAST], 8, hedge, 512 * MIB, 1, slow_block).await,
    );
}
