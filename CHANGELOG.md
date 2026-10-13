# Changelog

All notable changes to this project are documented here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- In the add dialog's review step, each package's file list folds away
  behind its row (it starts folded when there are more than a dozen
  files), with "Expand all" / "Collapse all" and a running total of
  packages, files and size. Folded rows still show the file count, size,
  options and warnings, and every package now says where it will be
  saved.

### Changed

- Each package now downloads into its own folder inside the download
  folder, instead of everything landing in one place. When an archive is
  extracted, the extracted files sit at the top of the package folder
  and the downloaded parts go in its `Archives` subfolder, which "delete
  archives" removes too. An archive that holds a single folder is
  unpacked without an extra level of nesting. A single file still goes
  straight into the download folder. Settings → "Put each package in its
  own folder" (on by default) turns this off; for `nuxd`, `--flat`.
  Downloads already in the queue keep their old layout.

### Fixed

- Removing a package together with its files no longer leaves files
  behind when one of them was just starting to download.

## [0.2.1]

### Added

- RAR archives are extracted, including multi-part sets (`.part1.rar`,
  `.r00`…), with the same safety checks as ZIP and 7z. Extraction uses
  the UnRAR library, which is freeware; its licence is under Settings →
  About.

## [0.2.0]

Paste a link to an album or gallery page and get the files in it; pick
exactly which ones to download.

### Added

- Paste a link to a web page (an album, a gallery, a folder listing) and
  get the files it links to or shows, instead of the page itself. An
  option also looks one level deeper, in the same site's pages it links
  to, for albums where every file has its own page; those pages are
  opened in parallel, so a deep scan of a large album takes seconds, not
  minutes. Pages that ask for a browser check are reported, not worked
  around. Pages that only show their files after running scripts can't
  be read.
- Pick exactly which files to add: every file in the review step has its
  own checkbox, with a select-all per package, and quick filters by type
  (videos, images, audio, archives, documents) to tick or untick a whole
  kind in one click, or keep "only" one.
- Redesigned checkboxes.
- Clearer logs: every failed attempt with its cause, files that give
  up, and (with `NUXD_LOG=debug`) a per-second speed trace.

### Fixed

- Files from servers that answer every range request with "the rest of
  the file" now download. They used to be refused piece by piece, so
  such a file never started and eventually failed.
- A file whose server answers every request with the same error now
  fails within seconds, with that error as the reason, instead of
  retrying every piece several times over minutes ("too many failed
  attempts").
- A file that keeps failing no longer takes the connection from
  downloads that work, or keeps queued downloads from starting.
- The speeds shown are measured by the engine over the last 3 seconds,
  so a steady download reads steady instead of jumping up and down.

## [0.1.1]

### Fixed

- A queue of many small files on a slow disk no longer piles up open
  files. A finished file stays open until it has been safely written and
  renamed; new files now wait while 16 finished ones are still queued for
  that, instead of the number growing without limit.

## [0.1.0]

The first release, for Linux (x86-64): AppImage, `.deb`, `.rpm` and the
`nuxd` command. Windows and macOS builds will follow once they have been
tested.

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
- The AppImage updates itself from GitHub Releases; `.deb`, `.rpm` and
  Arch installs are updated by the package manager.

### Command line

- `nuxd get`, `add`, `queue`, `resume`, `pause`, `remove`, `priority`,
  `move`, `limit`, `options`, `restart-file`, `links` and `diagnostics`,
  sharing the app's queue.

[Unreleased]: https://github.com/sidmax7/nux-downloader/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/sidmax7/nux-downloader/releases/tag/v0.2.1
[0.2.0]: https://github.com/sidmax7/nux-downloader/releases/tag/v0.2.0
[0.1.1]: https://github.com/sidmax7/nux-downloader/releases/tag/v0.1.1
[0.1.0]: https://github.com/sidmax7/nux-downloader/releases/tag/v0.1.0
