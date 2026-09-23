# Nux Downloader (nuxd)

> **Status: pre-alpha.** Nothing is functional yet. This is scaffolding
> for a project under active early development — expect the engine,
> CLI and UI described below to not exist yet in the code.

A download manager combining two ideas from separate prior-art tools:

- **[Plexo](https://github.com/anmolkapil/plexo)** — splits *one* file
  across every available network interface at once for maximum
  throughput.
- **JDownloader** — manages *many* files and links as grouped
  "packages," with a plugin-style link-resolution layer.

Nux Downloader combines both: one global scheduler hands byte ranges
from any file in any package to whichever network link is free, so a
single download saturates every interface and a queue of many files
never sits idle waiting on one slow connection. Files are written
straight into their final position on disk, with no separate
reassembly pass.

## Scope

Nux Downloader resolves and downloads **already-accessible direct
URLs**, plus simple redirect and short-link resolution. It does not
bypass captchas or anti-bot challenges, circumvent DRM or stream-
protection measures, or automate premium-account credential sharing.

## Building

Nothing to build yet. The workspace is Rust only:

- `core/` — the download engine, a library with no UI or Tauri
  dependency.
- `cli/` — a thin `nuxd` binary over `core`, for exercising the engine
  without the UI.
- `src-tauri/` / `ui/` — the desktop app (Tauri + Vue), not yet
  scaffolded.

```sh
cargo build --workspace
```

## Licence

Apache-2.0. See [LICENSE](LICENSE).
