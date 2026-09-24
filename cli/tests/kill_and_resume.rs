//! The flagship M1 test (`IMPLEMENTATION_PLAN.md` P2.10): run `nuxd` as a
//! real child process, `kill -9` it mid-download, restart, and check the
//! result is byte-exact. This is the end-to-end proof that the
//! durability protocol (§9.3) and resume (§9.4) actually survive a real
//! process death, not just a dropped `Driver` inside the same test binary
//! (see `core/tests/driver.rs` for that narrower, faster-running check).
//!
//! Uses `tokio::process::Command`, not `std::process::Command`: this test
//! also runs the testkit origin in-process on the same (default,
//! single-threaded) test runtime, and a blocking `std::process::Child`
//! wait would starve that runtime — including the origin's own task —
//! for as long as the child runs, deadlocking the whole test.

use nuxd_testkit::{Fault, Origin, OriginOptions, content_range};
use std::process::Stdio;
use std::time::Duration;
use tempfile::tempdir;
use tokio::process::Command;

fn nuxd() -> Command {
    Command::new(env!("CARGO_BIN_EXE_nuxd"))
}

#[tokio::test]
async fn kill_nine_mid_download_then_resume_is_byte_exact() {
    let size: u64 = 200_000;
    let seed = 77;
    let origin = Origin::start(OriginOptions {
        size,
        seed,
        ..Default::default()
    })
    .await;
    // Request #1 is the probe; request #2 is the transfer's only attempt
    // (200 KB stays under the 4 MiB single-block threshold). Trickling it
    // keeps the child process alive long enough to `kill -9` mid-body —
    // a real network would do this on its own, localhost needs help.
    origin.set_fault(2, Fault::Crawl(60_000));

    let data_dir = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let dest_dir = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));

    let mut child = nuxd()
        .args(["--data-dir"])
        .arg(data_dir.path())
        // A small write buffer so durable progress (and so a checkpoint)
        // shows up well before the file finishes, even trickled slowly —
        // the default 256 KiB buffer wouldn't flush at all for a file
        // this size before the kill below.
        .args(["--write-buffer-bytes", "8192"])
        // The origin is on loopback, which no discovered link reaches.
        .arg("--unbound")
        .args(["get", &origin.url(), "-o"])
        .arg(dest_dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|error| panic!("couldn't spawn nuxd: {error}"));

    // Long enough for several buffer's worth to have flushed (and
    // checkpointed), short enough to land well before the full crawl —
    // at ~60 KB/s, 200 KB takes a bit over 3 s.
    tokio::time::sleep(Duration::from_secs(2)).await;

    child
        .kill()
        .await
        .unwrap_or_else(|error| panic!("kill -9 failed: {error}"));
    let _ = child.wait().await;

    let packages_dir = data_dir.path().join("packages");
    let manifest_path = std::fs::read_dir(&packages_dir)
        .unwrap_or_else(|error| panic!("read_dir {packages_dir:?}: {error}"))
        .filter_map(|entry| entry.ok())
        .find(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("json"))
        .unwrap_or_else(|| panic!("no package manifest was ever checkpointed before the kill"))
        .path();
    let manifest = nuxd_core::storage::manifest::load_package_manifest(&manifest_path)
        .unwrap_or_else(|error| panic!("load manifest: {error}"));
    let durable_at_kill: u64 = manifest.package.files[0]
        .parts
        .iter()
        .map(|part| part.durable)
        .sum();
    assert!(
        durable_at_kill > 0,
        "expected at least one checkpoint to have landed before the kill"
    );
    assert!(
        durable_at_kill < size,
        "expected the kill to land mid-download, not after completion"
    );

    // Resume: the crawl fault was one-shot on request #2, so this run's
    // request(s) get the default, unthrottled fault and finish quickly.
    let status = nuxd()
        .args(["--data-dir"])
        .arg(data_dir.path())
        .arg("--unbound")
        .arg("resume")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .unwrap_or_else(|error| panic!("couldn't run nuxd resume: {error}"));
    assert!(status.success(), "nuxd resume exited with {status}");

    let files: Vec<_> = std::fs::read_dir(dest_dir.path())
        .unwrap_or_else(|error| panic!("read_dir: {error}"))
        .filter_map(|entry| entry.ok())
        .collect();
    assert_eq!(
        files.len(),
        1,
        "expected exactly one downloaded file, found {files:?}"
    );
    let bytes = std::fs::read(files[0].path()).unwrap_or_else(|error| panic!("read: {error}"));
    assert_eq!(
        bytes,
        content_range(seed, 0, size),
        "resumed download is not byte-exact"
    );

    origin.shutdown().await;
}
