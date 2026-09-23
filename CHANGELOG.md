# Changelog

All notable changes to this project are documented here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
this project adheres to [Semantic Versioning](https://semver.org/) once
it has a first release.

## [Unreleased]

### Added

- Cargo workspace scaffolding: `core` (`nuxd-core`, the engine library)
  and `cli` (`nuxd`, the CLI binary), both empty.
- Project tooling: pinned toolchain, workspace lints (`unwrap`/`expect`/
  `unsafe_code` denied outside tests), rustfmt and EditorConfig settings,
  CI workflow (fmt, clippy, tests on Linux/macOS/Windows, `cargo-deny`).
- Apache-2.0 licence.
