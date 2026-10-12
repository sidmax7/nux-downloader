# Benchmarks

Real-hardware throughput checks (`IMPLEMENTATION_PLAN.md` P3.10). Each
entry says exactly what was measured, so results stay comparable.

## Method

`nuxd get --links <links> <url> -o <dir>` for a fixed 20 seconds
(`timeout 20`), then the bytes that reached disk are read with
`du -sm` on the download directory. The engine writes in place into a
preallocated file, so `du` counts only blocks actually written. The
window includes connection start-up, so rates read slightly low. The
20-second runs were stopped, not completed; the completed run below
was checked with `bzip2 -t`.

## Linux — Ethernet + Wi-Fi to a phone hotspot (2026-09-24)

- Links: `enp8s0` (Ethernet, home router) and `wlan0` (Wi-Fi joined to a
  phone's mobile-data hotspot) — different gateways, so independent
  uplinks.
- File: Valve's public Steam Deck recovery image, 3,357,999,306 bytes,
  served with range support and no request rate limit
  (`host refused: 0` in every run).
- 8 connections per link (default), 8 MiB blocks.

| Links | Written in 20 s | Rate |
|---|---|---|
| `enp8s0` | 2087 MB | ≈ 104 MB/s |
| `wlan0` | 177 MB | ≈ 8.9 MB/s |
| `enp8s0` + `wlan0` | 2316 MB | ≈ 116 MB/s |

Sum of the single-link runs: 2264 MB; the combined run reached 102 % of
it, so combining links lost no throughput. Ethernet alone is close to
gigabit line rate (~117 MB/s), so the combined figure is bounded by the
two links' capacity, and the hotspot's ~9 MB/s is where the gain comes
from.

### Completed run, integrity-checked

The full file over both links: 3.13 GiB in 36.7 s (≈ 87 MB/s average,
including start-up and the tail). `enp8s0` carried 2.78 GiB and `wlan0`
360 MiB (≈ 11 %). `bzip2 -tv` on the result reported `ok` — the format's
own CRCs confirm the multi-link download is byte-correct.

Not yet measured: macOS (Wi-Fi + iPhone USB) and Windows (Wi-Fi +
Ethernet).

## Servers that rate-limit requests

Hetzner's and OVH's public test files answered `429` after a handful of
back-to-back requests per IP (OVH: about nine 1 MB range requests, then
`429` with `Retry-After: 10`), so throughput there measures the server's
limit, not the links. `nuxd` now backs off per (host, link) and honors
`Retry-After`, but these hosts are not suitable for benchmarking.


## Shaped loopback links (P7.3, P7.5)

Harness: `core/tests/bench.rs` (`#[ignore]`d; `cargo test --release -p
nuxd-core --test bench -- --ignored --nocapture --test-threads=1`). The
real engine downloads from the testkit origin over loopback aliases
(`127.0.0.N`), each one a **link**: one byte rate shared by all its
connections plus a fixed latency (`Origin::shape_source`). Links here are
40 MiB/s @ 20 ms ("fast") and 10 MiB/s @ 60 ms ("slow"), 8 connections
each. *Efficiency* = ideal time (bytes ÷ summed link rates) ÷ measured
time. *Idle slot-s* = connection slots offered over the run window minus
time requests were in flight. Run on the Linux dev machine, 2026-10; wall
clock, so ±2 % noise. Loopback has no loss, no TCP ramp-up over distance
and no ISP shaping — these numbers rank settings against each other, they
are not predictions of internet speed.

### One 128 MiB file: one link versus two (40 MiB/s @ 20 ms, 10 MiB/s @ 60 ms)
| Run | Time | Ideal | Efficiency | Idle slot-s | Requests |
|---|---|---|---|---|---|
| fast only | 3.20 s | 3.20 s | 100% | 0.1 | 33 |
| slow only | 12.88 s | 12.80 s | 99% | 0.5 | 33 |
| fast + slow | 2.66 s | 2.56 s | 96% | 0.1 | 41 |
### One 256 MiB file on the fast link: block size
| Run | Time | Ideal | Efficiency | Idle slot-s | Requests |
|---|---|---|---|---|---|
| 1 MiB blocks | 6.41 s | 6.40 s | 100% | 0.4 | 257 |
| 2 MiB blocks | 6.41 s | 6.40 s | 100% | 0.4 | 129 |
| 4 MiB blocks | 6.40 s | 6.40 s | 100% | 0.2 | 65 |
| 8 MiB blocks | 6.40 s | 6.40 s | 100% | 0.0 | 33 |
| 16 MiB blocks | 6.41 s | 6.40 s | 100% | 0.0 | 17 |
### One 128 MiB file over fast + slow links: hedge_after
| Run | Time | Ideal | Efficiency | Idle slot-s | Requests |
|---|---|---|---|---|---|
| hedge after 1 s | 2.66 s | 2.56 s | 96% | 0.1 | 41 |
| hedge after 2 s | 3.24 s | 2.56 s | 79% | 6.8 | 33 |
| hedge after 4 s | 3.24 s | 2.56 s | 79% | 6.8 | 33 |
| hedge after 8 s | 3.24 s | 2.56 s | 79% | 6.8 | 33 |
| no hedging | 3.24 s | 2.56 s | 79% | 6.8 | 33 |
### 20 × 8 MiB files over fast + slow links: hedge_after
| Run | Time | Ideal | Efficiency | Idle slot-s | Requests |
|---|---|---|---|---|---|
| hedge after 1 s | 3.38 s | 3.20 s | 95% | 2.7 | 180 |
| hedge after 2 s | 3.38 s | 3.20 s | 95% | 2.8 | 180 |
| hedge after 8 s | 3.38 s | 3.20 s | 95% | 2.7 | 180 |
### 200 × 100 KiB files on the fast link: slots per link (1 = sequential)
| Run | Time | Ideal | Efficiency | Idle slot-s | Requests |
|---|---|---|---|---|---|
| 1 slot(s) | 5.78 s | 0.49 s | 8% | 0.1 | 400 |
| 2 slot(s) | 2.90 s | 0.49 s | 17% | 0.1 | 400 |
| 4 slot(s) | 1.47 s | 0.49 s | 33% | 0.1 | 400 |
| 8 slot(s) | 0.74 s | 0.49 s | 66% | 0.2 | 400 |
| 16 slot(s) | 0.75 s | 0.49 s | 65% | 6.2 | 400 |
### One 512 MiB file, one 8 MiB block served at 200 kB/s: refresh versus hedge
| Run | Time | Ideal | Efficiency | Idle slot-s | Requests |
|---|---|---|---|---|---|
| neither | 42.43 s | 12.80 s | 30% | 208.9 | 65 |
| refresh only (3 s warm-up + 5 s) | 12.77 s | 12.80 s | 100% | 2.8 | 66 |
| refresh only (2 s warm-up + 3 s) | 12.81 s | 12.80 s | 100% | 3.2 | 66 |
| refresh only (1 s warm-up + 2 s) | 12.91 s | 12.80 s | 99% | 3.1 | 66 |
| hedge only (default) | 12.98 s | 12.80 s | 99% | 2.3 | 66 |
### One 32 MiB file, one block served at 100 kB/s: tail handling
| Run | Time | Ideal | Efficiency | Idle slot-s | Requests |
|---|---|---|---|---|---|
| neither | 10.41 s | 0.80 s | 8% | 67.7 | 33 |
| refresh only (3 s warm-up + 5 s) | 10.41 s | 0.80 s | 8% | 67.7 | 33 |
| hedge only, after 1 s | 1.34 s | 0.80 s | 60% | 4.1 | 34 |
| hedge only, after 2 s | 2.09 s | 0.80 s | 38% | 9.3 | 34 |
| hedge only, after 4 s | 4.32 s | 0.80 s | 19% | 25.0 | 34 |
| hedge only, after 8 s | 10.41 s | 0.80 s | 8% | 67.7 | 33 |
| both (defaults) | 1.34 s | 0.80 s | 60% | 4.1 | 34 |

### What the numbers say

- **Block size does not matter** on this model (1–16 MiB all within 0.2 %),
  so the planner's `clamp(size/32, 1 MiB, 8 MiB)` stays.
- **Slots per link:** 8 slots finish 200 small files in 0.74 s; 16 is no
  faster (and idles 6 slot-seconds); 1 slot — i.e. one file after another —
  takes 5.78 s, **7.8× slower**. 8 stays the default.
- **Aggregation:** fast + slow reached 96 % of the summed rate once tail
  hedging was tuned (below); with the old 8 s hedge timer it was 79 %,
  because the slow link's last blocks outlived the fast link's work.
- **Hedging:** the hedge timer is the dominant tail parameter — a block
  crawling at 100 kB/s finished in 1.34 s with `hedge_after` = 1 s, 2.09 s
  at 2 s, 4.32 s at 4 s and not at all (10.4 s, no benefit) at the old 8 s,
  because a block with < `hedge_after` left is judged not worth racing.
  Cost: 1 s adds a handful of duplicate requests at the very end (33 → 41
  requests on the 128 MiB two-link run); hedges only use slots that would
  otherwise idle. The default moved from 8 s to **1 s**.
- **Refresh** needs peers on the link: with a 200 kB/s block beside 8 MiB
  blocks at full speed, a 512 MiB file takes 42.4 s with no tail handling
  and 12.8 s (ideal) with refresh at the default 3 s warm-up + 5 s window;
  shorter windows (2+3 s, 1+2 s) gained nothing, so the defaults stay. (The
  benchmark caught a real defect on the way: the first version compared a
  connection with its peers' own windows, but peers are replaced every few
  seconds and never had a full window. It now compares with the link's
  per-connection fair share.)

Not measured here: real internet paths, macOS/Windows, TLS cost, and
Plexo — `scripts/realworld-bench.sh` does the curl and sequential
comparisons on real hardware; the Plexo number has to be timed by hand.
