# Changelog

All notable changes to this project are documented here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.1.0]

The first release. Linux is the tested platform; the Windows and macOS
builds are new and have had less testing.

### Downloading

- Splits one download across every network connection at once (Wi-Fi,
  Ethernet, a phone over USB), each connection bound to its own
  interface. A slow or failing connection is taken out of rotation and
  retried later; the last pieces of a file are raced on a second
  connection so one slow server doesn't hold up the end.
- Many downloads at once, grouped into packages, in a queue with
  priorities, reordering, pause and resume.
- Crash-safe: progress is saved as it goes, files are written in place,
  and after a crash, power loss or quit the download continues where it
  stopped. A full disk or a missing folder pauses the package with the
  reason; resume continues.
- Speed limits overall, per network connection and per package.
- Expired signed links are re-resolved, redirects followed, and a file
  that changes on the server is detected instead of being mixed.
- Networks that exist only inside the machine (Docker, VM bridges) are
  ignored unless named.

### Archives

- Multi-part archives (`.part1.rar`, `.7z.001`, `.zip.001`…) are grouped
  into one package; missing parts are pointed out before anything is
  queued.
- Checksum files (`.sha256`, `.md5`, `.sfv`…) are verified.
- Two separate options per package: extract after download (ZIP and 7z),
  and delete the archive after a successful extraction.

### Desktop app

- Add links by pasting anywhere in the window; review names, sizes and
  archive groups before queueing.
- Queue view with progress, speed and time left, a per-file block map
  coloured by connection, and a Links page to rename, recolour, switch
  off or limit each connection.
- Tray icon (pause all, resume all, quit), notifications, light and dark
  themes, one window per user (a second launch focuses the first).
- Updates itself from GitHub Releases (Windows, macOS, Linux AppImage);
  `.deb`, `.rpm` and AUR installs are updated by the package manager.

### Command line

- `nuxd get`, `add`, `queue`, `resume`, `pause`, `remove`, `priority`,
  `move`, `limit`, `options`, `restart-file`, `links` and `diagnostics`,
  sharing the app's queue.

[Unreleased]: https://github.com/sidmax7/nux-downloader/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/sidmax7/nux-downloader/releases/tag/v0.1.0
