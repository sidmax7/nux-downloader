//! Host policies on real sockets (`IMPLEMENTATION_PLAN.md` P4.3): the
//! testkit counts concurrent requests per source address, so a host's
//! connection and active-file limits can be checked against what the
//! origin actually saw, not just what the engine intended.

use nuxd_core::config::{EngineConfig, HostLimits};
use nuxd_core::driver::Driver;
use nuxd_core::engine::sim::one_link;
use nuxd_core::engine::{Command, EngineEvent, EngineState};
use nuxd_core::model::{Link, LinkId, LinkKind, Priority};
use nuxd_core::net::bind::BindPolicy;
use nuxd_core::net::monitor::LinkSetup;
use nuxd_testkit::{Fault, Origin, OriginOptions, content_range};
use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;
use tempfile::tempdir;

fn config_with(limits: HostLimits) -> EngineConfig {
    let mut config = EngineConfig::default();
    // The origin is `127.0.0.1`.
    config.hosts.add_rule("127.0.0.1", limits);
    config
}

async fn run(driver: &Driver, dest: &std::path::Path, sources: Vec<String>) {
    let mut events = driver.subscribe();
    driver
        .command(Command::AddPackage {
            name: "pkg".to_string(),
            dest_dir: dest.to_path_buf(),
            sources,
            priority: Priority::Normal,
            post: Default::default(),
        })
        .await
        .unwrap_or_else(|e| panic!("AddPackage: {e}"));
    tokio::time::timeout(Duration::from_secs(60), async {
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
    })
    .await
    .unwrap_or_else(|_| panic!("never finished"));
}

#[tokio::test]
async fn a_hosts_connection_limit_is_never_exceeded() {
    let size: u64 = 24 * 1024 * 1024;
    let origin = Origin::start(OriginOptions {
        size,
        seed: 51,
        ..Default::default()
    })
    .await;
    origin.set_default_fault(Fault::Throttle(20_000_000));
    let dest = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let manifests = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let driver = Driver::spawn_fresh(
        config_with(HostLimits {
            max_connections_per_link: 2,
            max_active_files: None,
        }),
        vec![one_link()], // 8 slots — the host limit is what holds it to 2
        manifests.path().to_path_buf(),
    );
    run(&driver, dest.path(), vec![origin.url()]).await;

    let source = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let peak = origin.peak_concurrent_from(source);
    assert!(peak <= 2, "host allows 2 per link, origin saw {peak}");
    assert!(
        peak >= 2,
        "…and the limit should actually be used, saw {peak}"
    );
    let file = std::fs::read_dir(dest.path())
        .unwrap_or_else(|e| panic!("read_dir: {e}"))
        .filter_map(Result::ok)
        .next()
        .unwrap_or_else(|| panic!("no file"));
    assert_eq!(
        std::fs::read(file.path()).unwrap_or_else(|e| panic!("read: {e}")),
        content_range(51, 0, size)
    );
    driver.shutdown().await;
    origin.shutdown().await;
}

#[tokio::test]
async fn the_limit_applies_per_link_not_in_total() {
    let size: u64 = 24 * 1024 * 1024;
    let origin = Origin::start(OriginOptions {
        size,
        seed: 52,
        ..Default::default()
    })
    .await;
    origin.set_default_fault(Fault::Throttle(20_000_000));
    let dest = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let manifests = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let links: Vec<Link> = [2u8, 3]
        .iter()
        .map(|&n| Link {
            id: LinkId::new(u64::from(n)),
            name: format!("lo{n}"),
            label: format!("lo{n}"),
            kind: LinkKind::Other,
            v4: Some(Ipv4Addr::new(127, 0, 0, n)),
            v6: None,
            max_slots: 8,
            limit: None,
        })
        .collect();
    let driver = Driver::spawn_with(
        EngineState::new(config_with(HostLimits {
            max_connections_per_link: 3,
            max_active_files: None,
        })),
        Vec::new(),
        LinkSetup::Fixed {
            links,
            bind: BindPolicy::Address,
        },
        manifests.path().to_path_buf(),
    );
    run(&driver, dest.path(), vec![origin.url()]).await;

    for n in [2u8, 3] {
        let peak = origin.peak_concurrent_from(IpAddr::V4(Ipv4Addr::new(127, 0, 0, n)));
        assert_eq!(
            peak, 3,
            "each link may hold 3 to the host; .{n} peaked at {peak}"
        );
    }
    driver.shutdown().await;
    origin.shutdown().await;
}

#[tokio::test]
async fn max_active_files_holds_files_back_and_all_still_finish() {
    let size: u64 = 300_000;
    let origin = Origin::start(OriginOptions {
        size,
        seed: 53,
        ..Default::default()
    })
    .await;
    origin.set_default_fault(Fault::Throttle(2_000_000));
    let dest = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let manifests = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let driver = Driver::spawn_fresh(
        config_with(HostLimits {
            max_connections_per_link: 8,
            max_active_files: Some(2),
        }),
        vec![one_link()],
        manifests.path().to_path_buf(),
    );
    // Six single-block files: at most two may be transferring at once.
    run(&driver, dest.path(), (0..6).map(|_| origin.url()).collect()).await;

    // Transfers only: the pool's own probes (`Range: bytes=0-0`) also
    // reach the host, but are not active files.
    let peak = origin.max_overlap(|r| {
        r.range
            .is_none_or(|range| !(range.start == 0 && range.end == Some(0)))
    });
    assert!(peak <= 2, "host allows 2 active files, origin saw {peak}");
    let finished = std::fs::read_dir(dest.path())
        .unwrap_or_else(|e| panic!("read_dir: {e}"))
        .filter_map(Result::ok)
        .count();
    assert_eq!(finished, 6);
    driver.shutdown().await;
    origin.shutdown().await;
}
