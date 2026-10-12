# Hardening (Phase 8)

What was tried to break `nuxd`, what broke, and what is still open. The
commands to repeat each run are in the test and script headers.

## Fuzzing (P8.1)

`fuzz/` holds five `cargo-fuzz` targets: `headers` (Content-Range,
Content-Disposition, filename sanitizer), `grouping`, `entry_paths`
(archive entry names), `manifests` (queue and package JSON), `text_parsers`
(checksum sidecars, rates, URL redaction, host rules). Each asserts
properties, not just "no panic" (e.g. a sanitized name is never empty, never
contains a separator, is idempotent; every input lands in exactly one
group; an entry path can never leave its root; a manifest that loads
serializes stably).

First runs of 45 s – 4 min per target found **five** real defects, all
fixed with a regression unit test next to the code:

| Found by | Defect | Fix |
|---|---|---|
| `headers` | `parse_content_range` sliced a `&str` at byte 5 and panicked inside a multi-byte character | `str::get` |
| `grouping` | `x.part4294967295.rar` made the gap list try to allocate 4 billion entries (out of memory) | gaps listed lazily, capped at 1000 |
| `headers` | a sanitized name cut at 255 bytes could end in a space/dot (silently stripped by Windows) | truncate first, then trim |
| `text_parsers` | a checksum-sidecar line naming `dir/` produced an entry with an empty file name | entries with no file name are skipped |
| review of the above | the placeholder name made from a URL before probing was not sanitized | sanitized |

After the fixes every target ran clean for 2–4 minutes (manifests: 15 M
executions). **Not done: the "hours without findings" bar** — that needs
machine time. `scripts/fuzz-all.sh 3600` runs each target for an hour.

## Chaos and soak (P8.2)

- `cli/tests/chaos.rs` — `kill -9`s the real `nuxd` at random moments
  while the server flaps (connections blackholed, cut, truncated, delayed,
  500, 429), restarting until each batch finishes, then compares every file
  byte for byte. 120 s run: 18 rounds, 61 kills, all byte-exact; the
  child's peak RSS stayed at 18 MiB and its descriptors at 27.
- `core/tests/soak.rs` — one long-lived engine under churn (4-file
  packages added and removed, weather changing every round). 90 s run: 112
  rounds, RSS 18 → 17 MiB, descriptors 24 → 24, every file byte-exact.

`NUXD_CHAOS_SECS=86400` / `NUXD_SOAK_SECS=86400` give the plan's 24-hour
run; **it has not been run** (60–120 s each so far).

The chaos run found one real defect: a kill between reserving a file name
and the first checkpoint left an empty placeholder (and its sparse
`.nuxdpart`) that nothing referred to, because the name only reached a
manifest at the next 5-second checkpoint. A checkpoint is now written the
moment a file is prepared; the window shrinks from seconds to the time one
manifest write takes. **Not eliminated:** a kill inside that window can
still leave an orphan placeholder. Orphans are never touched automatically
(they can't be told from another program's files).

## Edge cases (P8.3)

| Case | Behaviour | Test |
|---|---|---|
| Disk full mid-download | The package **pauses with the reason** (`paused: … No space left on device`); partial data kept; `resume` reopens the files and finishes byte-exact | `cli/tests/disk_full.rs` (a real 4 MiB tmpfs in a user namespace) + 2 sim tests |
| Destination deleted mid-download | The lost file is marked failed (`restart-file` refetches it); the package never claims success | `edge_cases.rs` |
| Permission denied | Pauses with the OS reason; resumes after `chmod` | `edge_cases.rs` |
| Non-UTF-8 destination path | **Was broken**: the manifest could not be written, silently losing the queue on restart. Paths are now stored losslessly | `edge_cases.rs`, `path_serde` tests |
| Corrupt package manifest | **Was silently skipped.** Now renamed `*.json.corrupt` and reported by `nuxd diagnostics` | manifest test |
| Server ends every response early | The file fails after its retries; no hang | `edge_cases.rs` |
| Files over 100 GiB | 120 GiB file: ≤ 4096 blocks, sparse placeholder (no real space used), manifest resumes. On a disk without room it is refused up front with the reason | `edge_cases.rs` |
| Two `nuxd` on one data directory | The second is refused ("another nuxd is using …"); the OS drops a dead process's lock | `lock.rs`, `cli/tests/queue.rs` |
| Windows long paths (`\\?\`) | **Not handled and not testable here.** Paths over 260 characters will fail on Windows until the storage layer uses extended-length paths | open |

## Security review (P8.4)

Reviewed by reading the code against these threats; the Tauri IPC surface
and updater do not exist yet and are reviewed with P6.4.

Fixed:
- **Symlink at the partial-file path.** `.nuxdpart` was created with
  create+truncate, so a symlink planted in a shared download folder would
  have been written through. It is now created exclusively (never follows a
  link); a stale regular file is replaced, a link or directory is refused.
  Resume refuses a symlinked partial file too.
- **Predictable extraction staging directory.** A symlink planted at
  `.name.nuxdextract` could have steered extraction elsewhere. The path is
  now cleared as a link (never followed) and created exclusively.
- **https → http redirect downgrade** is refused (the file could be
  swapped in transit). Redirects stay capped at 5.
- **Secrets in logs and reports:** the diagnostics report and the pasted
  log tail cut every URL down to host and path (no query, fragment or
  `user:pass@`); default package names no longer include the query string.

Checked, no change needed: server-supplied file names (sanitizer, fuzzed;
reserved Windows names, `:` streams, separators, NUL, length); URL schemes
(only `http`/`https`; `file:`, `ftp:` refused); final-name placeholders
(`create_new`); archive extraction (traversal, absolute/drive/UNC paths,
symlinks never created, bomb limits, tested for zip, 7z and RAR); TLS
(rustls, no certificate-validation overrides anywhere); no response
decompression; sidecar size cap; deletion helpers only take plain names.

Accepted: someone who can write the data directory can already act as the
user (manifests are trusted). A hostile local user who can write the
*download* directory can still race the file operations between a check and
its use; the fixes above close the practical symlink cases.

## Performance profile (P8.5)

`cli/tests/profile.rs`, release build, loopback, no TLS, 12-core machine:

- **CPU:** 1 GiB at ~820 MB/s (6.6 Gbit/s): 0.28 s of CPU in `nuxd` =
  **≈ 0.03 cores per Gbit/s**; peak RSS **37 MiB**. (TLS adds its own
  cost, not measured here.)
- **Write pattern** (`strace`, 256 MiB, 8 connections): 653 `pwrite`s,
  mostly 256–460 KiB (the 256 KiB buffer plus the chunk that crossed it),
  18 `fsync`s (checkpoints), 8 renames.

**HDD vs SSD was not measured** (no spinning disk here). With 8 blocks
written in parallel, a hard disk sees 8 interleaved streams of ~0.4 MiB
writes, which will cost seeks; raising `--write-buffer-bytes` (e.g. 4 MiB)
is the knob to try there. Allocation profiling was not done (no
`valgrind`/`heaptrack`); the flat RSS in the soak run is the evidence we
have.

## Logging and diagnostics (P8.6)

`tracing` writes daily-rotated files to `<data dir>/logs/nuxd.*.log` (seven
kept; `NUXD_LOG=debug` for detail), warnings still go to stderr.
`nuxd diagnostics` (and `Engine::diagnostics()` for the app's "copy
diagnostics" button) print version, platform, free space, links, the queue
with failure reasons, and the last 150 log lines, with URLs reduced to
host and path. Nothing is sent anywhere; there is no telemetry.

## The app's security configuration (P6.4)

Reviewed against the Tauri v2 security guidance (`tauri.conf.json`,
`capabilities/default.json`). The review is ours, not an external audit.

- **Capabilities:** the window may only `listen`/`unlisten` to events. No
  shell, filesystem, HTTP, opener, dialog or notification permission is
  granted to the page; the Rust side uses the dialog and notification
  plugins itself (`pick_folder` is a command, so the page never gets a
  general file-system dialog API). Verified live: with this capability the
  window still receives its 4 Hz snapshots.
- **CSP:** `default-src 'self'`, scripts only from the app, no `object`,
  no framing, no forms, no `base`; `connect-src` only Tauri's IPC. The
  dev CSP additionally allows the Vite server on `localhost:1420`.
  `style-src` has `'unsafe-inline'` because Vue binds styles
  (`:style` widths); there is no inline script. `freezePrototype` is on.
- **No remote content:** the app loads nothing from the network; link
  checking and downloads happen in the Rust engine, never in the webview.
- **IPC surface:** every command takes plain values and returns readable
  strings on error; paths come from the user's own settings or the system
  folder chooser. Package and file names shown in the UI come from servers
  but are rendered as text (Vue escapes them; there is no `v-html`).
- **Not done:** the command list is not restricted with an app manifest
  (any script in the window could call any of our commands; the CSP is
  what keeps foreign scripts out). The updater (P9.5) will need its own
  review.
