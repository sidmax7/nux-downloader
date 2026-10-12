//! The link monitor wired into the driver (`IMPLEMENTATION_PLAN.md`
//! P3.4): links that appear and disappear while a download runs. The
//! interface list is a fake the test edits; clients are unbound (the
//! fake links have no real interface), so every connection still reaches
//! the loopback testkit origin.

use nuxd_core::config::EngineConfig;
use nuxd_core::driver::Driver;
use nuxd_core::engine::{Command, EngineEvent, EngineState};
use nuxd_core::model::Priority;
use nuxd_core::net::bind::BindPolicy;
use nuxd_core::net::discover::{LinkSource, RawAddr};
use nuxd_core::net::monitor::{LinkSetup, MonitorSetup};
use nuxd_testkit::{Fault, Origin, OriginOptions, content_range};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::tempdir;

#[derive(Default)]
struct FakeInterfaces(Mutex<Vec<RawAddr>>);

impl FakeInterfaces {
    fn set(&self, entries: &[(&str, &str)]) {
        let addrs = entries
            .iter()
            .map(|(name, ip)| RawAddr {
                name: (*name).to_string(),
                ip: ip
                    .parse()
                    .unwrap_or_else(|error| panic!("bad ip {ip}: {error}")),
            })
            .collect();
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = addrs;
    }
}

impl LinkSource for FakeInterfaces {
    fn addresses(&self) -> io::Result<Vec<RawAddr>> {
        Ok(self.0.lock().unwrap_or_else(|e| e.into_inner()).clone())
    }
}

fn monitored(interfaces: &Arc<FakeInterfaces>) -> LinkSetup {
    LinkSetup::Monitored(MonitorSetup {
        source: interfaces.clone(),
        enrichers: Vec::new(),
        interval: Duration::from_millis(50),
        enrich_timeout: Duration::from_secs(1),
        bind: BindPolicy::Off,
        only: None,
        switch: Default::default(),
    })
}

async fn add(driver: &Driver, dest: std::path::PathBuf, url: String) {
    driver
        .command(Command::AddPackage {
            name: "pkg".to_string(),
            dest_dir: dest,
            sources: vec![url],
            priority: Priority::Normal,
            post: Default::default(),
        })
        .await
        .unwrap_or_else(|error| panic!("couldn't send AddPackage: {error}"));
}

async fn wait_done(events: &mut tokio::sync::broadcast::Receiver<EngineEvent>) {
    tokio::time::timeout(Duration::from_secs(15), async {
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

fn only_file_in(dir: &std::path::Path) -> Vec<u8> {
    let files: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("read_dir: {error}"))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    assert_eq!(files.len(), 1, "expected one finalized file, got {files:?}");
    std::fs::read(&files[0]).unwrap_or_else(|error| panic!("read: {error}"))
}

#[tokio::test]
async fn a_download_waits_for_a_link_to_appear_then_completes() {
    let size = 300_000;
    let origin = Origin::start(OriginOptions {
        size,
        seed: 5,
        ..Default::default()
    })
    .await;
    let dest = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let manifests = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));

    let interfaces = Arc::new(FakeInterfaces::default());
    let driver = Driver::spawn_with(
        EngineState::new(EngineConfig::default()),
        Vec::new(),
        monitored(&interfaces),
        manifests.path().to_path_buf(),
    );
    let mut events = driver.subscribe();
    add(&driver, dest.path().to_path_buf(), origin.url()).await;

    // No links yet: nothing may finish.
    let early = tokio::time::timeout(Duration::from_millis(400), wait_done(&mut events)).await;
    assert!(early.is_err(), "finished without any link");
    assert!(
        driver
            .snapshot()
            .await
            .is_some_and(|s| s.links.is_empty() && !s.packages.is_empty())
    );

    interfaces.set(&[("eth9", "10.9.0.2")]);
    wait_done(&mut events).await;
    assert_eq!(only_file_in(dest.path()), content_range(5, 0, size));

    driver.shutdown().await;
    origin.shutdown().await;
}

#[tokio::test]
async fn losing_the_link_mid_transfer_moves_the_block_to_another_link() {
    let size = 300_000;
    let origin = Origin::start(OriginOptions {
        size,
        seed: 6,
        ..Default::default()
    })
    .await;
    // Request #1 is the probe; #2 is the transfer on the first link.
    // Crawling it guarantees it is still running when the link vanishes.
    origin.set_fault(2, Fault::Crawl(30_000));

    let dest = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let manifests = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));

    let interfaces = Arc::new(FakeInterfaces::default());
    interfaces.set(&[("eth9", "10.9.0.2")]);
    let driver = Driver::spawn_with(
        EngineState::new(EngineConfig::default()),
        Vec::new(),
        monitored(&interfaces),
        manifests.path().to_path_buf(),
    );
    let mut events = driver.subscribe();
    add(&driver, dest.path().to_path_buf(), origin.url()).await;

    tokio::time::sleep(Duration::from_millis(500)).await;
    // eth9 goes away; usb9 takes over.
    interfaces.set(&[("usb9", "10.9.1.2")]);

    wait_done(&mut events).await;
    assert_eq!(only_file_in(dest.path()), content_range(6, 0, size));

    let snapshot = driver
        .snapshot()
        .await
        .unwrap_or_else(|| panic!("no snapshot"));
    assert_eq!(snapshot.links.len(), 1);
    assert_eq!(snapshot.links[0].link.name, "usb9");

    driver.shutdown().await;
    origin.shutdown().await;
}
