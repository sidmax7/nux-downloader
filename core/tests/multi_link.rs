//! Several links at once (`IMPLEMENTATION_PLAN.md` P3.9). Each "link" is
//! a loopback alias (`127.0.0.2`, `.3`, `.4`) that connections bind to by
//! source address; the testkit attributes every request to the address it
//! came from, so it can tell the links apart, slow one down, or cut one
//! off. On macOS CI adds the `lo0` aliases first.

use nuxd_core::config::EngineConfig;
use nuxd_core::driver::Driver;
use nuxd_core::engine::{Command, EngineEvent, EngineSnapshot, EngineState};
use nuxd_core::model::{Link, LinkId, LinkKind, Priority};
use nuxd_core::net::bind::BindPolicy;
use nuxd_core::net::health::LinkState;
use nuxd_core::net::monitor::LinkSetup;
use nuxd_testkit::{Fault, Origin, OriginOptions, content_range};
use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;
use tempfile::tempdir;

fn ip(last: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(127, 0, 0, last))
}

fn loopback_link(last: u8, slots: u8) -> Link {
    Link {
        id: LinkId::new(u64::from(last)),
        name: format!("lo{last}"),
        label: format!("loopback .{last}"),
        kind: LinkKind::Other,
        v4: Some(Ipv4Addr::new(127, 0, 0, last)),
        v6: None,
        max_slots: slots,
        limit: None,
    }
}

fn setup(lasts: &[u8], slots: u8) -> LinkSetup {
    LinkSetup::Fixed {
        links: lasts.iter().map(|&n| loopback_link(n, slots)).collect(),
        bind: BindPolicy::Address,
    }
}

fn fast_health_config() -> EngineConfig {
    EngineConfig {
        link_degrade_backoff_min: Duration::from_millis(200),
        link_degrade_backoff_max: Duration::from_millis(400),
        // A blackholed request is noticed after this long.
        stall_timeout: Duration::from_millis(500),
        ..EngineConfig::default()
    }
}

fn start(config: EngineConfig, links: LinkSetup, manifests: &std::path::Path) -> Driver {
    Driver::spawn_with(
        EngineState::new(config),
        Vec::new(),
        links,
        manifests.to_path_buf(),
    )
}

async fn add(driver: &Driver, dest: &std::path::Path, url: String) {
    driver
        .command(Command::AddPackage {
            name: "pkg".to_string(),
            dest_dir: dest.to_path_buf(),
            sources: vec![url],
            priority: Priority::Normal,
        })
        .await
        .unwrap_or_else(|error| panic!("couldn't send AddPackage: {error}"));
}

async fn wait_done(events: &mut tokio::sync::broadcast::Receiver<EngineEvent>) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match events.recv().await {
                Ok(EngineEvent::PackageDone { .. }) => return,
                Ok(
                    event @ (EngineEvent::PackageFailed { .. } | EngineEvent::FileFailed { .. }),
                ) => {
                    panic!("download failed: {event:?}")
                }
                Ok(_) => {}
                Err(error) => panic!("events channel error: {error}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("package never finished"));
}

async fn snapshot(driver: &Driver) -> EngineSnapshot {
    driver
        .snapshot()
        .await
        .unwrap_or_else(|| panic!("no snapshot"))
}

async fn wait_until<F: Fn(&EngineSnapshot) -> bool>(driver: &Driver, what: &str, condition: F) {
    let waited = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if condition(&snapshot(driver).await) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await;
    assert!(waited.is_ok(), "timed out waiting for: {what}");
}

fn downloaded(dest: &std::path::Path) -> Vec<u8> {
    let files: Vec<_> = std::fs::read_dir(dest)
        .unwrap_or_else(|error| panic!("read_dir: {error}"))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    assert_eq!(files.len(), 1, "expected one finalized file, got {files:?}");
    std::fs::read(&files[0]).unwrap_or_else(|error| panic!("read: {error}"))
}

/// How many requests each address made.
fn requests_from(origin: &Origin, source: IpAddr) -> usize {
    origin
        .requests()
        .iter()
        .filter(|request| request.from.ip() == source)
        .count()
}

fn bytes_from(origin: &Origin, source: IpAddr) -> u64 {
    origin
        .requests()
        .iter()
        .filter(|request| request.from.ip() == source)
        .map(|request| {
            request
                .bytes_sent
                .load(std::sync::atomic::Ordering::Relaxed)
        })
        .sum()
}

fn link_stats(snapshot: &EngineSnapshot, last: u8) -> nuxd_core::engine::LinkStats {
    snapshot
        .links
        .iter()
        .find(|stats| stats.link.id == LinkId::new(u64::from(last)))
        .cloned()
        .unwrap_or_else(|| panic!("no link .{last} in snapshot"))
}

#[tokio::test]
async fn one_file_is_shared_across_three_links() {
    let size: u64 = 24 * 1024 * 1024;
    let origin = Origin::start(OriginOptions {
        size,
        seed: 11,
        ..Default::default()
    })
    .await;
    // Slow enough that no single link can finish before the others have
    // started their own blocks: 3 × 2 × 5 MB/s ≈ 30 MB/s, ~1 s in total.
    for last in [2, 3, 4] {
        origin.set_source_fault(ip(last), Fault::Throttle(5_000_000));
    }
    let dest = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let manifests = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let driver = start(
        EngineConfig::default(),
        setup(&[2, 3, 4], 2),
        manifests.path(),
    );
    let mut events = driver.subscribe();
    add(&driver, dest.path(), origin.url()).await;
    wait_done(&mut events).await;

    assert_eq!(downloaded(dest.path()), content_range(11, 0, size));
    for last in [2, 3, 4] {
        assert!(
            requests_from(&origin, ip(last)) >= 2,
            "link .{last} should have carried blocks"
        );
        assert!(
            origin.peak_concurrent_from(ip(last)) <= 2,
            "link .{last} exceeded its 2-slot cap"
        );
    }

    // Attribution: per-link bytes add up to the whole file.
    let after = snapshot(&driver).await;
    let credited: u64 = after.links.iter().map(|stats| stats.bytes).sum();
    assert_eq!(credited, size);

    driver.shutdown().await;
    origin.shutdown().await;
}

#[tokio::test]
async fn a_faster_link_carries_more_of_the_file() {
    let size: u64 = 24 * 1024 * 1024;
    let origin = Origin::start(OriginOptions {
        size,
        seed: 12,
        ..Default::default()
    })
    .await;
    origin.set_source_fault(ip(2), Fault::Throttle(1_000_000)); // slow
    origin.set_source_fault(ip(3), Fault::Throttle(3_000_000)); // medium
    origin.set_source_fault(ip(4), Fault::Throttle(9_000_000)); // fast
    let dest = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let manifests = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let driver = start(
        EngineConfig::default(),
        setup(&[2, 3, 4], 1),
        manifests.path(),
    );
    let mut events = driver.subscribe();
    add(&driver, dest.path(), origin.url()).await;
    wait_done(&mut events).await;
    assert_eq!(downloaded(dest.path()), content_range(12, 0, size));

    let after = snapshot(&driver).await;
    let (slow, medium, fast) = (
        link_stats(&after, 2).bytes,
        link_stats(&after, 3).bytes,
        link_stats(&after, 4).bytes,
    );
    assert!(fast > medium, "fast {fast} vs medium {medium}");
    assert!(medium > slow, "medium {medium} vs slow {slow}");
    assert_eq!(slow + medium + fast, size);
    // The origin agrees about who did the work.
    assert!(bytes_from(&origin, ip(4)) > bytes_from(&origin, ip(2)));

    driver.shutdown().await;
    origin.shutdown().await;
}

#[tokio::test]
async fn a_link_that_is_dead_from_the_start_degrades_and_the_rest_finish() {
    let size: u64 = 12 * 1024 * 1024;
    let origin = Origin::start(OriginOptions {
        size,
        seed: 13,
        ..Default::default()
    })
    .await;
    origin.block_source(ip(3));
    for last in [2, 4] {
        origin.set_source_fault(ip(last), Fault::Throttle(10_000_000));
    }
    let dest = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let manifests = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let driver = start(fast_health_config(), setup(&[2, 3, 4], 2), manifests.path());
    let mut events = driver.subscribe();
    add(&driver, dest.path(), origin.url()).await;
    wait_done(&mut events).await;

    // The dead link never burned a block's retries: the file completed.
    assert_eq!(downloaded(dest.path()), content_range(13, 0, size));
    assert_eq!(requests_from(&origin, ip(3)), 0);
    let after = snapshot(&driver).await;
    assert_eq!(link_stats(&after, 3).bytes, 0);
    assert_eq!(
        link_stats(&after, 2).bytes + link_stats(&after, 4).bytes,
        size
    );

    driver.shutdown().await;
    origin.shutdown().await;
}

#[tokio::test]
async fn a_link_that_dies_mid_download_is_degraded_then_recovers() {
    let size: u64 = 32 * 1024 * 1024;
    let origin = Origin::start(OriginOptions {
        size,
        seed: 14,
        ..Default::default()
    })
    .await;
    for last in [2, 3, 4] {
        origin.set_source_fault(ip(last), Fault::Throttle(6_000_000));
    }
    let dest = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let manifests = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let driver = start(fast_health_config(), setup(&[2, 3, 4], 2), manifests.path());
    let mut events = driver.subscribe();
    add(&driver, dest.path(), origin.url()).await;

    // Let .3 carry some data, then cut it off from new connections.
    wait_until(&driver, "link .3 to deliver bytes", |s| {
        link_stats(s, 3).bytes > 0
    })
    .await;
    origin.block_source(ip(3));
    wait_until(&driver, "link .3 to degrade", |s| {
        matches!(link_stats(s, 3).state, LinkState::Degraded { .. })
    })
    .await;

    // The link comes back; a trial connection succeeds and it is Up again.
    let before = link_stats(&snapshot(&driver).await, 3).bytes;
    origin.unblock_source(ip(3));
    wait_until(&driver, "link .3 to recover", |s| {
        let stats = link_stats(s, 3);
        stats.state == LinkState::Up && stats.bytes > before
    })
    .await;

    wait_done(&mut events).await;
    assert_eq!(downloaded(dest.path()), content_range(14, 0, size));

    driver.shutdown().await;
    origin.shutdown().await;
}
