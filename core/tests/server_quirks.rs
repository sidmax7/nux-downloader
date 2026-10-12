//! Servers that answer differently from the textbook, found on real
//! downloads: ranges answered "to the end of the file" (Decision 31), a
//! protocol error on every block of a file (Decision 32), and a failing
//! file that must not take the connection from one downloading fine
//! (Decision 33).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use nuxd_core::config::EngineConfig;
use nuxd_core::engine::sim::one_link;
use nuxd_core::model::{FileError, FileStatus, PackageId, PackageStatus, Priority};
use nuxd_core::net::bind::BindPolicy;
use nuxd_core::net::monitor::LinkSetup;
use nuxd_core::{AddOptions, Engine};
use nuxd_testkit::{Fault, LoggedRequest, Origin, OriginOptions, content_range};
use std::path::Path;
use std::time::{Duration, Instant};
use tempfile::tempdir;

const MIB: u64 = 1024 * 1024;

/// 1 MiB blocks for anything over 512 KiB.
fn config() -> EngineConfig {
    EngineConfig {
        min_block_bytes: MIB,
        single_block_threshold_bytes: MIB / 2,
        ..EngineConfig::default()
    }
}

/// An engine on one link with `slots`.
async fn engine(data_dir: &Path, slots: u8) -> Engine {
    start(data_dir, slots, config()).await
}

async fn start(data_dir: &Path, slots: u8, config: EngineConfig) -> Engine {
    let mut link = one_link();
    link.max_slots = slots;
    Engine::start_with(
        config,
        Some(data_dir.to_path_buf()),
        LinkSetup::Fixed {
            links: vec![link],
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

fn is_probe(request: &LoggedRequest) -> bool {
    request
        .range
        .is_some_and(|r| r.start == 0 && r.end == Some(0))
}

/// Waits until `package` is done or failed; returns its status.
async fn settled(engine: &Engine, package: PackageId, within: Duration) -> PackageStatus {
    tokio::time::timeout(within, async {
        loop {
            let snapshot = engine.snapshot().await;
            if let Some(p) = snapshot.packages.iter().find(|p| p.id == package)
                && matches!(p.status, PackageStatus::Done | PackageStatus::Failed(_))
            {
                return p.status.clone();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the package never settled")
}

#[tokio::test]
async fn ranges_answered_to_the_end_of_the_file_still_download() {
    let size = 3 * MIB;
    let origin = Origin::start(OriginOptions {
        size,
        seed: 61,
        ..Default::default()
    })
    .await;
    origin.set_default_fault(Fault::RangeToEnd);
    let data = tempdir().unwrap();
    let dest = tempdir().unwrap();
    let engine = engine(data.path(), 2).await;
    let id = engine
        .add(vec![origin.url()], opts("p", dest.path()))
        .await
        .unwrap()[0];

    assert_eq!(
        settled(&engine, id, Duration::from_secs(20)).await,
        PackageStatus::Done
    );
    let file = std::fs::read_dir(dest.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    assert_eq!(
        std::fs::read(file.path()).unwrap(),
        content_range(61, 0, size)
    );
    // One request per block, each bounded, none of them retried.
    let transfers: Vec<_> = origin
        .requests()
        .into_iter()
        .filter(|r| !is_probe(r))
        .collect();
    assert_eq!(transfers.len(), 3, "{transfers:?}");
    engine.shutdown().await;
}

#[tokio::test]
async fn the_same_protocol_error_on_every_block_fails_the_file_at_once() {
    let origin = Origin::start(OriginOptions {
        size: 8 * MIB,
        seed: 62,
        ..Default::default()
    })
    .await;
    // The probe (request 1) is fine; every transfer is answered with a
    // range that starts one byte late.
    origin.set_default_fault(Fault::WrongStart);
    origin.set_fault(1, Fault::Ok);
    let data = tempdir().unwrap();
    let dest = tempdir().unwrap();
    let engine = engine(data.path(), 4).await;
    let id = engine
        .add(vec![origin.url()], opts("p", dest.path()))
        .await
        .unwrap()[0];

    let started = Instant::now();
    let status = settled(&engine, id, Duration::from_secs(20)).await;
    assert!(matches!(status, PackageStatus::Failed(_)), "{status:?}");
    // Without Decision 32 each of the 8 blocks retried 5 times with
    // backoffs (1 + 2 + 4 + 8 + 15 s) before the file gave up.
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    let transfers = origin.requests().iter().filter(|r| !is_probe(r)).count();
    assert!(transfers <= 8, "{transfers} requests before giving up");

    let snapshot = engine.snapshot().await;
    let file = &snapshot.packages[0].files[0];
    match &file.status {
        FileStatus::Failed(FileError::Other { detail }) => {
            assert!(detail.contains("wrong range"), "{detail}");
        }
        other => panic!("expected the server's error as the reason, got {other:?}"),
    }
    engine.shutdown().await;
}

#[tokio::test]
async fn a_failing_file_does_not_take_the_connection_from_a_healthy_one() {
    // `failing` is first in the queue and has eight 1 MiB blocks; every
    // transfer from it sends headers, then nothing, and is given up after
    // the 300 ms stall timeout (like the real host, whose failed requests
    // held the connection for 0.2–0.5 s each). `healthy` downloads in
    // four 1 MiB blocks at about 1 MB/s. There is one connection. (A
    // block that delivered nothing already avoids that link; it was the
    // failing file's *other*, untried blocks that kept taking the
    // connection, one after another.)
    let failing = Origin::start(OriginOptions {
        size: 8 * MIB,
        seed: 63,
        ..Default::default()
    })
    .await;
    failing.set_default_fault(Fault::StallBody);
    failing.set_fault(1, Fault::Ok);
    let healthy = Origin::start(OriginOptions {
        size: 4 * MIB,
        seed: 64,
        ..Default::default()
    })
    .await;
    healthy.set_default_fault(Fault::Throttle(1_000_000));

    let data = tempdir().unwrap();
    let dest = tempdir().unwrap();
    let engine = start(
        data.path(),
        1,
        EngineConfig {
            stall_timeout: Duration::from_millis(300),
            ..config()
        },
    )
    .await;
    engine
        .add(vec![failing.url()], opts("failing", dest.path()))
        .await
        .unwrap();
    let healthy_id = engine
        .add(vec![healthy.url()], opts("healthy", dest.path()))
        .await
        .unwrap()[0];
    assert_eq!(
        settled(&engine, healthy_id, Duration::from_secs(30)).await,
        PackageStatus::Done
    );

    // After its second failure the failing file is "troubled": it may not
    // start again while the healthy file still has blocks to fetch.
    let failing_starts: Vec<Instant> = failing
        .requests()
        .iter()
        .filter(|r| !is_probe(r))
        .map(|r| r.started)
        .collect();
    assert!(failing_starts.len() >= 2, "{failing_starts:?}");
    let troubled_since = failing_starts[1];
    let healthy_last_start = healthy
        .requests()
        .iter()
        .filter(|r| !is_probe(r))
        .map(|r| r.started)
        .max()
        .unwrap();
    let interrupting = failing_starts
        .iter()
        .filter(|&&at| at > troubled_since && at < healthy_last_start)
        .count();
    assert_eq!(
        interrupting, 0,
        "the failing file took the connection {interrupting} times while the healthy one still had blocks"
    );
    engine.shutdown().await;
}
