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
