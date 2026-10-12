//! Chaos: `kill -9` the real `nuxd` over and over while the server
//! misbehaves, and check every file still comes out byte-exact
//! (`IMPLEMENTATION_PLAN.md` P8.2).
//!
//! Ignored by default; the duration comes from `NUXD_CHAOS_SECS` (default
//! 60, the plan's soak is 86400):
//!
//! ```text
//! NUXD_CHAOS_SECS=86400 cargo test --release -p nuxd --test chaos -- --ignored --nocapture
//! ```
//!
//! Each round queues a batch of files, then runs `nuxd resume` and kills
//! it at a random moment until it finishes on its own. Meanwhile the
//! origin flaps: connections blackholed for a moment, requests cut,
//! truncated, delayed, refused with 500 or 429. After every round all files
//! are compared with what the server serves. The child's memory and open
//! file descriptors are sampled from `/proc` before each kill; the last
//! quarter of the run must not use meaningfully more than the first.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::cast_precision_loss)]

use nuxd_testkit::{Fault, Origin, OriginOptions, content_range};
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tempfile::tempdir;
use tokio::process::Command;

const FILE_SIZE: u64 = 6 * 1024 * 1024;
const FILES: usize = 6;
const SEED: u64 = 99;

/// A tiny deterministic generator, so a failing run can be replayed.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn proc_stats(pid: u32) -> Option<(u64, usize)> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let rss_kb: u64 = status
        .lines()
        .find(|l| l.starts_with("VmRSS:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    let fds = std::fs::read_dir(format!("/proc/{pid}/fd")).ok()?.count();
    Some((rss_kb / 1024, fds))
}

/// `restart-file` for every failed file; returns how many there were.
async fn restart_failed(data: &Path) -> usize {
    let queue = nuxd(data).args(["queue", "-v"]).output().await.unwrap();
    let text = String::from_utf8_lossy(&queue.stdout).into_owned();
    let mut count = 0;
    for line in text.lines().filter(|l| l.trim_start().starts_with("file ")) {
        let mut words = line.split_whitespace();
        let (Some("file"), Some(id), Some(status)) = (words.next(), words.next(), words.next())
        else {
            continue;
        };
        if status == "failed:" || status == "failed" {
            let _ = nuxd(data).args(["restart-file", id]).output().await;
            count += 1;
        }
    }
    count
}

fn nuxd(data: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nuxd"));
    command.arg("--data-dir").arg(data).arg("--unbound");
    command
}

#[tokio::test]
#[ignore = "chaos run: see the module docs"]
async fn kill_nine_and_flapping_server() {
    let seconds: u64 = std::env::var("NUXD_CHAOS_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);
    let seed: u64 = std::env::var("NUXD_CHAOS_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0x5eed);
    let mut rng = Rng(seed | 1);
    eprintln!("chaos: {seconds} s, seed {seed}");

    let origin = Origin::start(OriginOptions {
        size: FILE_SIZE,
        seed: SEED,
        ..Default::default()
    })
    .await;
    let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
    origin.shape_source(loopback, 24 * 1024 * 1024, Duration::from_millis(5));

    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut samples: Vec<(u64, usize)> = Vec::new();
    let (mut rounds, mut kills) = (0u32, 0u32);

    while Instant::now() < deadline {
        rounds += 1;
        let data = tempdir().unwrap();
        let dest = tempdir().unwrap();
        let mut add = nuxd(data.path());
        add.arg("add").arg("-o").arg(dest.path());
        for i in 0..FILES {
            add.arg(format!("{}?n={i}", origin.url()));
        }
        assert!(add.output().await.unwrap().status.success());

        let round_started = Instant::now();
        loop {
            assert!(
                round_started.elapsed() < Duration::from_secs(300),
                "round {rounds} is stuck"
            );
            // Weather for this run of the child.
            match rng.below(6) {
                0 => origin.set_default_fault(Fault::Ok),
                1 => origin.set_default_fault(Fault::CutAfter(100_000 + rng.below(500_000))),
                2 => origin.set_default_fault(Fault::EndAfter(50_000 + rng.below(500_000))),
                3 => origin.set_default_fault(Fault::Delay(rng.below(400))),
                4 => origin.set_default_fault(Fault::Status(500)),
                _ => origin.set_default_fault(Fault::RateLimited(1)),
            }
            // A heal after a while, so rounds do finish.
            let mut child = nuxd(data.path())
                .arg("resume")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let life = Duration::from_millis(300 + rng.below(2500));
            let ends = Instant::now() + life;
            let mut finished = None;
            while Instant::now() < ends {
                if let Ok(Some(status)) = child.try_wait() {
                    finished = Some(status);
                    break;
                }
                if rng.below(8) == 0 {
                    origin.block_source(loopback);
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    origin.unblock_source(loopback);
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            match finished {
                Some(status) => {
                    // Under hostile weather files can fail (a probe got a
                    // 500, retries ran out): restart them and go again.
                    let restarted = restart_failed(data.path()).await;
                    if status.success() && restarted == 0 {
                        break;
                    }
                }
                None => {
                    if let Some(pid) = child.id()
                        && let Some(stats) = proc_stats(pid)
                    {
                        samples.push(stats);
                    }
                    child.kill().await.unwrap();
                    let _ = child.wait().await;
                    kills += 1;
                }
            }
            // Later in a round the weather clears, so it converges.
            if round_started.elapsed() > Duration::from_secs(20) {
                origin.set_default_fault(Fault::Ok);
            }
        }

        // Byte-exact, every file.
        let expected = content_range(SEED, 0, FILE_SIZE);
        let files: Vec<_> = std::fs::read_dir(dest.path())
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        if files.len() != FILES {
            let queue = nuxd(data.path())
                .args(["queue", "-v"])
                .output()
                .await
                .unwrap();
            panic!(
                "round {rounds}: {} of {FILES} files\n{}",
                files.len(),
                String::from_utf8_lossy(&queue.stdout)
            );
        }
        for file in files {
            let name = file.file_name().to_string_lossy().into_owned();
            assert!(!name.ends_with(".nuxdpart"), "leftover {name}");
            assert!(
                std::fs::read(file.path()).unwrap() == expected,
                "round {rounds}: {name} is corrupt (seed {seed})"
            );
        }
        eprintln!(
            "chaos: round {rounds} ok after {:.1}s ({kills} kills so far)",
            round_started.elapsed().as_secs_f64()
        );
        origin.set_default_fault(Fault::Ok);
    }

    assert!(rounds >= 1 && kills >= 1, "nothing was exercised");
    if samples.len() >= 8 {
        let quarter = samples.len() / 4;
        let peak = |part: &[(u64, usize)]| {
            (
                part.iter().map(|s| s.0).max().unwrap_or(0),
                part.iter().map(|s| s.1).max().unwrap_or(0),
            )
        };
        let (first_rss, first_fds) = peak(&samples[..quarter]);
        let (last_rss, last_fds) = peak(&samples[samples.len() - quarter..]);
        eprintln!(
            "chaos: {rounds} rounds, {kills} kills; child peak RSS {first_rss} → {last_rss} MiB, fds {first_fds} → {last_fds}"
        );
        assert!(
            last_rss <= first_rss * 3 / 2 + 32,
            "memory grew: {first_rss} → {last_rss} MiB"
        );
        assert!(
            last_fds <= first_fds + 16,
            "descriptors grew: {first_fds} → {last_fds}"
        );
    } else {
        eprintln!("chaos: {rounds} rounds, {kills} kills (too few samples to compare)");
    }
}
