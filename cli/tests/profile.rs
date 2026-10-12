//! CPU and write-pattern profiling (`IMPLEMENTATION_PLAN.md` P8.5): the
//! real `nuxd` downloads from an origin in this process while its own CPU
//! time is read from `/proc`, so only the downloader is measured.
//!
//! Ignored by default (it moves a lot of data):
//!
//! ```text
//! cargo test --release -p nuxd --test profile -- --ignored --nocapture
//! ```

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::cast_precision_loss)]

use nuxd_testkit::{Origin, OriginOptions};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tempfile::tempdir;
use tokio::process::Command;

/// utime + stime of `pid`, in seconds, from `/proc/<pid>/stat`.
fn cpu_seconds(pid: u32) -> Option<f64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields after the parenthesised command name.
    let rest = stat.rsplit_once(')')?.1;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let utime: f64 = fields.get(11)?.parse().ok()?;
    let stime: f64 = fields.get(12)?.parse().ok()?;
    Some((utime + stime) / 100.0) // clock ticks at 100 Hz
}

#[tokio::test]
#[ignore = "profiling run: see the module docs"]
async fn cpu_per_gigabit_and_write_pattern() {
    const SIZE: u64 = 1024 * 1024 * 1024;
    let origin = Origin::start(OriginOptions {
        size: SIZE,
        seed: 5,
        ..Default::default()
    })
    .await;
    let data = tempdir().unwrap();
    let dest = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();

    let started = Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_nuxd"))
        .arg("--data-dir")
        .arg(data.path())
        .arg("--unbound")
        .arg("get")
        .arg(origin.url())
        .arg("-o")
        .arg(dest.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id().unwrap();
    let mut last_cpu = 0.0;
    let mut peak_rss_mib = 0u64;
    let status = loop {
        if let Some(cpu) = cpu_seconds(pid) {
            last_cpu = cpu;
        }
        if let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status"))
            && let Some(kb) = status
                .lines()
                .find(|l| l.starts_with("VmHWM:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<u64>().ok())
        {
            peak_rss_mib = peak_rss_mib.max(kb / 1024);
        }
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let wall = started.elapsed().as_secs_f64();
    assert!(status.success());

    let gib = SIZE as f64 / (1024.0 * 1024.0 * 1024.0);
    let rate = SIZE as f64 / wall;
    let gbps = rate * 8.0 / 1e9;
    let cores = last_cpu / wall;
    eprintln!(
        "profile: {gib:.1} GiB in {wall:.2} s = {:.0} MB/s ({gbps:.2} Gbit/s); nuxd CPU {last_cpu:.2} s \
         = {:.2} s per GiB = {:.3} cores per Gbit/s; peak RSS {peak_rss_mib} MiB",
        rate / 1e6,
        last_cpu / gib,
        cores / gbps,
    );
}

/// How the file is written, from `strace`: write sizes and how often the
/// data is synced. (Needs `strace`; says so and passes without it.)
#[tokio::test]
#[ignore = "profiling run: see the module docs"]
async fn write_pattern() {
    if Command::new("strace").arg("-V").output().await.is_err() {
        eprintln!("skipped: strace is not installed");
        return;
    }
    const SIZE: u64 = 256 * 1024 * 1024;
    let origin = Origin::start(OriginOptions {
        size: SIZE,
        seed: 6,
        ..Default::default()
    })
    .await;
    let data = tempdir().unwrap();
    let dest = tempdir().unwrap();
    let log = data.path().join("strace.log");
    let status = Command::new("strace")
        .args([
            "-f",
            "-qq",
            "-e",
            "trace=pwrite64,write,fdatasync,fsync,rename",
            "-o",
        ])
        .arg(&log)
        .arg(env!("CARGO_BIN_EXE_nuxd"))
        .arg("--data-dir")
        .arg(data.path())
        .arg("--unbound")
        .arg("get")
        .arg(origin.url())
        .arg("-o")
        .arg(dest.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .unwrap();
    assert!(status.success());

    let text = std::fs::read_to_string(&log).unwrap();
    let mut sizes: std::collections::BTreeMap<u64, u32> = std::collections::BTreeMap::new();
    let (mut syncs, mut renames) = (0u32, 0u32);
    for line in text.lines() {
        let call = line.split_once(' ').map_or(line, |(_, rest)| rest);
        if call.starts_with("pwrite64(") {
            // pwrite64(fd, "…"..., SIZE, OFFSET) = N
            let args = call.rsplit_once(") = ").map_or(call, |(a, _)| a);
            let parts: Vec<&str> = args.rsplitn(3, ", ").collect();
            if let Some(size) = parts.get(1).and_then(|s| s.parse::<u64>().ok()) {
                *sizes.entry(size).or_default() += 1;
            }
        } else if call.starts_with("fdatasync(") || call.starts_with("fsync(") {
            syncs += 1;
        } else if call.starts_with("rename(") {
            renames += 1;
        }
    }
    let total: u32 = sizes.values().sum();
    let bytes: u64 = sizes.iter().map(|(s, n)| s * u64::from(*n)).sum();
    let mut common: Vec<_> = sizes.iter().collect();
    common.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
    eprintln!(
        "write pattern: {total} pwrite calls carrying {} MiB; most common sizes (bytes × count): {:?}; \
         {syncs} syncs, {renames} renames",
        bytes >> 20,
        &common[..common.len().min(4)]
    );
}
