//! The M3 integration suite (`IMPLEMENTATION_PLAN.md` P4.8): many files
//! and packages through the whole engine on real sockets, with the
//! testkit's request log as the witness.

use nuxd_core::config::EngineConfig;
use nuxd_core::engine::sim::one_link;
use nuxd_core::model::{PackageStatus, Priority};
use nuxd_core::net::bind::BindPolicy;
use nuxd_core::net::monitor::LinkSetup;
use nuxd_core::{AddOptions, Engine};
use nuxd_testkit::{Fault, LoggedRequest, Origin, OriginOptions, content_range};
use std::time::{Duration, Instant};
use tempfile::tempdir;

const SLOTS: usize = 8;

fn is_probe(request: &LoggedRequest) -> bool {
    request
        .range
        .is_some_and(|r| r.start == 0 && r.end == Some(0))
}

fn opts(name: &str, dest: &std::path::Path) -> AddOptions {
    AddOptions {
        name: name.to_string(),
        dest_dir: dest.to_path_buf(),
        priority: Priority::Normal,
        extract: false,
        delete_archives: false,
    }
}

async fn engine(data_dir: &std::path::Path, config: EngineConfig) -> Engine {
    let mut link = one_link();
    link.max_slots = u8::try_from(SLOTS).unwrap_or(8);
    Engine::start_with(
        config,
        Some(data_dir.to_path_buf()),
        LinkSetup::Fixed {
            links: vec![link],
            bind: BindPolicy::Off,
        },
    )
    .await
    .unwrap_or_else(|e| panic!("start: {e}"))
}

async fn wait_all_done(engine: &Engine, expected_packages: usize) {
    let ok = tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            let snapshot = engine.snapshot().await;
            if snapshot.packages.len() == expected_packages
                && snapshot
                    .packages
                    .iter()
                    .all(|p| p.status == PackageStatus::Done)
            {
                return;
            }
            assert!(
                !snapshot
                    .packages
                    .iter()
                    .any(|p| matches!(p.status, PackageStatus::Failed(_))),
                "a package failed: {:?}",
                snapshot
                    .packages
                    .iter()
                    .map(|p| &p.status)
                    .collect::<Vec<_>>()
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await;
    assert!(ok.is_ok(), "the queue never finished");
}

fn all_files_match(dir: &std::path::Path, seed: u64, size: u64, expected_count: usize) {
    let expected = content_range(seed, 0, size);
    let mut count = 0;
    for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir: {e}")) {
        let path = entry.unwrap_or_else(|e| panic!("entry: {e}")).path();
        assert!(
            path.extension().is_none_or(|x| x != "nuxdpart"),
            "leftover partial file {path:?}"
        );
        assert_eq!(
            std::fs::read(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}")),
            expected,
            "{path:?}"
        );
        count += 1;
    }
    assert_eq!(count, expected_count, "files in {dir:?}");
}

#[tokio::test]
async fn two_hundred_small_files_use_exactly_two_hundred_transfer_requests() {
    let size = 5_000;
    let origin = Origin::start(OriginOptions {
        size,
        seed: 101,
        ..Default::default()
    })
    .await;
    let data_dir = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let dest = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let config = EngineConfig {
        max_probes_per_host: 8,
        ..EngineConfig::default()
    };
    let engine = engine(data_dir.path(), config).await;
    engine
        .add(
            (0..200).map(|_| origin.url()).collect(),
            opts("small", dest.path()),
        )
        .await
        .unwrap_or_else(|e| panic!("add: {e}"));
    wait_all_done(&engine, 1).await;

    all_files_match(dest.path(), 101, size, 200);
    let requests = origin.requests();
    let transfers: Vec<_> = requests.iter().filter(|r| !is_probe(r)).collect();
    assert_eq!(
        transfers.len(),
        200,
        "one request per small file, no splitting, no retries"
    );
    assert!(
        transfers.iter().all(|r| r.range.is_none()),
        "single-block files are fetched whole, without a Range header"
    );
    assert_eq!(requests.iter().filter(|r| is_probe(r)).count(), 200);
    engine.shutdown().await;
    origin.shutdown().await;
}

/// Time-weighted concurrency of `intervals` over `[from, to]`: the
/// fraction of that window during which at least `slots` were in flight.
fn full_fraction(
    intervals: &[(Instant, Instant)],
    from: Instant,
    to: Instant,
    slots: usize,
) -> f64 {
    let mut edges: Vec<(Instant, i32)> = Vec::new();
    for &(start, end) in intervals {
        edges.push((start, 1));
        edges.push((end, -1));
    }
    edges.sort_by_key(|(at, delta)| (*at, *delta));
    let (mut current, mut full) = (0i32, Duration::ZERO);
    let mut last = from;
    for (at, delta) in edges {
        let clamped = at.clamp(from, to);
        if usize::try_from(current).unwrap_or(0) >= slots {
            full += clamped.saturating_duration_since(last);
        }
        last = clamped;
        current += delta;
    }
    let window = to.saturating_duration_since(from).as_secs_f64();
    if window == 0.0 {
        1.0
    } else {
        full.as_secs_f64() / window
    }
}

#[tokio::test]
async fn a_fifty_volume_set_and_two_hundred_small_files_keep_every_slot_busy() {
    let volume_size: u64 = 1024 * 1024;
    let small_size: u64 = 100_000;
    let volumes = Origin::start(OriginOptions {
        size: volume_size,
        seed: 102,
        ..Default::default()
    })
    .await;
    let smalls = Origin::start(OriginOptions {
        size: small_size,
        seed: 103,
        ..Default::default()
    })
    .await;
    // Slow enough that a slot is genuinely busy for tens of milliseconds.
    volumes.set_default_fault(Fault::Throttle(4_000_000));
    smalls.set_default_fault(Fault::Throttle(4_000_000));

    let data_dir = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let dest_volumes = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let dest_smalls = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let config = EngineConfig {
        // 1 MiB volumes split into four 256 KiB blocks; 100 KB files don't.
        min_block_bytes: 256 * 1024,
        single_block_threshold_bytes: 512 * 1024,
        max_probes_per_host: 8,
        max_concurrent_probes: 16,
        ..EngineConfig::default()
    };
    let engine = engine(data_dir.path(), config).await;
    engine
        .add(
            (0..50).map(|_| volumes.url()).collect(),
            opts("volumes", dest_volumes.path()),
        )
        .await
        .unwrap_or_else(|e| panic!("add volumes: {e}"));
    engine
        .add(
            (0..200).map(|_| smalls.url()).collect(),
            opts("small", dest_smalls.path()),
        )
        .await
        .unwrap_or_else(|e| panic!("add smalls: {e}"));
    wait_all_done(&engine, 2).await;

    all_files_match(dest_volumes.path(), 102, volume_size, 50);
    all_files_match(dest_smalls.path(), 103, small_size, 200);

    // Exactly the requests needed: 4 per volume, 1 per small file.
    let mut transfers: Vec<LoggedRequest> = Vec::new();
    for origin in [&volumes, &smalls] {
        transfers.extend(origin.requests().into_iter().filter(|r| !is_probe(r)));
    }
    let volume_requests = volumes.requests().iter().filter(|r| !is_probe(r)).count();
    let small_requests = smalls.requests().iter().filter(|r| !is_probe(r)).count();
    assert_eq!(
        volume_requests,
        50 * 4,
        "each volume is four block requests"
    );
    assert_eq!(small_requests, 200, "each small file is one request");

    // No idle slots except at the queue's tail: from the first transfer
    // until fewer requests remain than there are slots, all slots are busy.
    let now = Instant::now();
    let mut starts: Vec<Instant> = transfers.iter().map(|r| r.started).collect();
    starts.sort();
    let intervals: Vec<(Instant, Instant)> = transfers
        .iter()
        .map(|r| {
            let end = r.finished.lock().ok().and_then(|f| *f).unwrap_or(now);
            (r.started, end)
        })
        .collect();
    // Skip the ramp-up: the window opens when the slots first fill.
    let opens = starts[SLOTS - 1];
    let closes = starts[starts.len() - SLOTS];
    let busy = full_fraction(&intervals, opens, closes, SLOTS);
    assert!(
        busy >= 0.85,
        "slots were full only {:.0}% of the steady-state window",
        busy * 100.0
    );

    // Per-source peak never exceeded the slot count.
    let peak = transfers_overlap(&intervals);
    assert!(
        peak <= SLOTS,
        "peak concurrency {peak} exceeds {SLOTS} slots"
    );
    engine.shutdown().await;
    volumes.shutdown().await;
    smalls.shutdown().await;
}

fn transfers_overlap(intervals: &[(Instant, Instant)]) -> usize {
    let mut edges: Vec<(Instant, i32)> = Vec::new();
    for &(start, end) in intervals {
        edges.push((start, 1));
        edges.push((end, -1));
    }
    edges.sort_by_key(|(at, delta)| (*at, *delta));
    let (mut current, mut peak) = (0i32, 0i32);
    for (_, delta) in edges {
        current += delta;
        peak = peak.max(current);
    }
    usize::try_from(peak).unwrap_or(0)
}

#[tokio::test]
async fn a_checkpoint_rewrites_only_the_packages_that_changed() {
    let quick = Origin::start(OriginOptions {
        size: 3_000,
        seed: 104,
        ..Default::default()
    })
    .await;
    let slow = Origin::start(OriginOptions {
        size: 2 * 1024 * 1024,
        seed: 105,
        ..Default::default()
    })
    .await;
    slow.set_default_fault(Fault::Throttle(1_000_000)); // ~2 s
    let data_dir = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let dest = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let config = EngineConfig {
        checkpoint_interval: Duration::from_millis(150),
        write_buffer_bytes: 16 * 1024,
        ..EngineConfig::default()
    };
    let engine = engine(data_dir.path(), config).await;

    // A package that finishes at once and then sits idle…
    let done = engine
        .add(vec![quick.url()], opts("done", &dest.path().join("done")))
        .await
        .unwrap_or_else(|e| panic!("add: {e}"));
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if engine
                .snapshot()
                .await
                .packages
                .iter()
                .any(|p| p.id == done[0] && p.status == PackageStatus::Done)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("first package never finished"));
    let done_manifest =
        nuxd_core::driver::prepare::manifest_path(&data_dir.path().join("packages"), done[0]);
    let before = std::fs::metadata(&done_manifest)
        .and_then(|m| m.modified())
        .unwrap_or_else(|e| panic!("stat: {e}"));

    // …while another downloads for a while (many checkpoint intervals).
    let busy = engine
        .add(vec![slow.url()], opts("slow", &dest.path().join("slow")))
        .await
        .unwrap_or_else(|e| panic!("add: {e}"));
    let busy_manifest =
        nuxd_core::driver::prepare::manifest_path(&data_dir.path().join("packages"), busy[0]);
    tokio::time::sleep(Duration::from_millis(1200)).await;

    let after = std::fs::metadata(&done_manifest)
        .and_then(|m| m.modified())
        .unwrap_or_else(|e| panic!("stat: {e}"));
    assert_eq!(
        before, after,
        "an unchanged package's manifest must not be rewritten"
    );
    let busy_after = std::fs::metadata(&busy_manifest)
        .and_then(|m| m.modified())
        .unwrap_or_else(|e| panic!("stat: {e}"));
    assert!(
        busy_after > before,
        "the downloading package's manifest is kept current"
    );

    engine.shutdown().await;
    quick.shutdown().await;
    slow.shutdown().await;
}
