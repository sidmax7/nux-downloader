//! Logging to rotating files and the "copy diagnostics" report
//! (`IMPLEMENTATION_PLAN.md` P8.6). No telemetry: nothing here leaves the
//! machine unless the user copies the report themselves, so the report
//! is built to be safe to paste into a bug report — URLs lose their
//! query strings and credentials (signed links carry secrets there).

use crate::config::EngineConfig;
use crate::engine::LinkStats;
use crate::model::{Package, PackageStatus};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

const KEEP_LOG_FILES: usize = 7;
const LOG_TAIL_LINES: usize = 150;

/// The platform data directory `nuxd` uses when none is given.
#[must_use]
pub fn default_data_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "nuxd").map(|dirs| dirs.data_dir().to_path_buf())
}

/// Where a data directory's logs go.
#[must_use]
pub fn log_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("logs")
}

/// Keeps buffered log lines flowing to disk; hold it until exit.
pub struct LoggingGuard {
    _file: Option<tracing_appender::non_blocking::WorkerGuard>,
}

/// Installs the global `tracing` subscriber: daily-rotated files in
/// `log_dir` (the last seven kept) at `info` for nuxd's own crates — or
/// whatever `NUXD_LOG` says — and, when `stderr` is set, warnings (or
/// `RUST_LOG`) on standard error. Safe to call once; a second call, or a
/// log directory that can't be created, leaves logging as it was.
pub fn init_logging(log_dir: &Path, stderr: bool) -> LoggingGuard {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{EnvFilter, Layer};

    let file = std::fs::create_dir_all(log_dir)
        .ok()
        .and_then(|()| {
            tracing_appender::rolling::Builder::new()
                .rotation(tracing_appender::rolling::Rotation::DAILY)
                .filename_prefix("nuxd")
                .filename_suffix("log")
                .max_log_files(KEEP_LOG_FILES)
                .build(log_dir)
                .ok()
        })
        .map(tracing_appender::non_blocking);
    let (file_layer, guard) = match file {
        Some((writer, guard)) => {
            let filter = EnvFilter::try_from_env("NUXD_LOG")
                .unwrap_or_else(|_| EnvFilter::new("warn,nuxd_core=info,nuxd=info"));
            let layer = tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(writer)
                .with_filter(filter);
            (Some(layer), Some(guard))
        }
        None => (None, None),
    };
    let stderr_layer = stderr.then(|| {
        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
        tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_filter(filter)
    });
    let _ = tracing_subscriber::registry()
        .with(file_layer)
        .with(stderr_layer)
        .try_init();
    LoggingGuard { _file: guard }
}

/// Cuts every URL in `text` down to scheme, host and path: no user info,
/// no query, no fragment.
#[must_use]
pub fn redact_urls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = ["http://", "https://"]
        .iter()
        .filter_map(|scheme| rest.find(scheme).map(|i| (i, scheme.len())))
        .min_by_key(|(i, _)| *i)
    {
        let (start, scheme_len) = at;
        out.push_str(&rest[..start]);
        let tail = &rest[start + scheme_len..];
        let end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | ')' | ']'))
            .unwrap_or(tail.len());
        let token = &tail[..end];
        out.push_str(&rest[start..start + scheme_len]);
        // user:password@host
        let authority_end = token.find(['/', '?', '#']).unwrap_or(token.len());
        let host_start = token[..authority_end].rfind('@').map_or(0, |i| i + 1);
        let cut = token.find(['?', '#']);
        out.push_str(&token[host_start..cut.unwrap_or(token.len())]);
        if host_start > 0 || cut.is_some() {
            out.push('…');
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// What the report describes.
pub struct ReportInput<'a> {
    pub data_dir: &'a Path,
    pub config: Option<&'a EngineConfig>,
    pub packages: &'a [Package],
    pub links: &'a [LinkStats],
    /// Manifest files that couldn't be read.
    pub unreadable: Vec<String>,
}

/// A plain-text report safe to paste into a bug report.
#[must_use]
pub fn render(input: &ReportInput<'_>) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "nuxd diagnostics");
    let _ = writeln!(out, "version: {}", env!("CARGO_PKG_VERSION"));
    let _ = writeln!(
        out,
        "platform: {} {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    let _ = writeln!(out, "data dir: {}", input.data_dir.display());
    if let Ok(free) = fs4::available_space(input.data_dir) {
        let _ = writeln!(out, "free space there: {} MiB", free >> 20);
    }
    if let Some(config) = input.config {
        let _ = writeln!(
            out,
            "config: slots/link {}, total connections {}, probes {} (per host {}), \
             hedge after {:?}, global limit {:?}",
            config.slots_per_link,
            config.max_connections_total,
            config.max_concurrent_probes,
            config.max_probes_per_host,
            config.hedge_after,
            config
                .global_limit
                .map(crate::model::ByteRate::bytes_per_sec),
        );
    }

    let _ = writeln!(out, "\nlinks ({}):", input.links.len());
    for stats in input.links {
        let _ = writeln!(
            out,
            "  {} [{:?}] {:?}, {} active, {} B/s, {} B total",
            stats.link.name,
            stats.link.kind,
            stats.state,
            stats.active,
            stats.speed as u64,
            stats.bytes
        );
    }

    let _ = writeln!(out, "\npackages ({}):", input.packages.len());
    for package in input.packages {
        let done = package
            .files
            .iter()
            .filter(|f| f.status == crate::model::FileStatus::Verified)
            .count();
        let status = match (&package.status, &package.last_error) {
            (PackageStatus::Failed(error), _) => format!("failed: {error}"),
            (PackageStatus::Paused, Some(error)) => format!("paused: {error}"),
            (other, _) => format!("{other:?}"),
        };
        let _ = writeln!(
            out,
            "  #{} {:?}: {status}; {done}/{} files; extract={} delete_archives={}",
            package.id,
            package.name,
            package.files.len(),
            package.extract,
            package.delete_archives,
        );
        for file in package
            .files
            .iter()
            .filter(|f| matches!(f.status, crate::model::FileStatus::Failed(_)))
        {
            if let crate::model::FileStatus::Failed(error) = &file.status {
                let _ = writeln!(out, "      file {:?}: {error}", file.name);
            }
        }
    }
    for name in &input.unreadable {
        let _ = writeln!(out, "  unreadable manifest: {name}");
    }

    let tail = log_tail(&log_dir(input.data_dir), LOG_TAIL_LINES);
    if !tail.is_empty() {
        let _ = writeln!(out, "\nrecent log:\n{tail}");
    }
    redact_urls(&out)
}

/// The report for a data directory, read from disk without starting an
/// engine (so it works while a download is running elsewhere).
#[must_use]
pub fn from_disk(data_dir: &Path) -> String {
    let dir = data_dir.join("packages");
    let mut packages = Vec::new();
    let mut unreadable = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".json.corrupt") {
                unreadable.push(name);
            } else if path.extension().and_then(|e| e.to_str()) == Some("json") {
                match crate::storage::manifest::load_package_manifest(&path) {
                    Ok(manifest) => packages.push(manifest.package),
                    Err(_) => unreadable.push(name),
                }
            }
        }
    }
    packages.sort_by_key(|p| p.id);
    render(&ReportInput {
        data_dir,
        config: None,
        packages: &packages,
        links: &[],
        unreadable,
    })
}

/// The last `lines` lines of the newest log file in `dir`.
fn log_tail(dir: &Path, lines: usize) -> String {
    let newest = std::fs::read_dir(dir)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("nuxd"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .max()
        .map(|(_, path)| path);
    let Some(path) = newest else {
        return String::new();
    };
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_lose_queries_credentials_and_fragments() {
        let text = "failed https://user:secret@cdn.example.com/a/b.bin?token=SIGNED&x=1#frag, \
                    retrying (http://plain.example/p?k=v) done";
        let out = redact_urls(text);
        assert!(!out.contains("secret"), "{out}");
        assert!(!out.contains("SIGNED"), "{out}");
        assert!(!out.contains("frag"), "{out}");
        assert!(out.contains("https://cdn.example.com/a/b.bin…"), "{out}");
        assert!(out.contains("http://plain.example/p…)"), "{out}");
        assert!(out.ends_with("done"));
    }

    #[test]
    fn text_without_urls_and_clean_urls_pass_through() {
        assert_eq!(redact_urls("nothing here"), "nothing here");
        assert_eq!(
            redact_urls("see https://example.com/file.zip now"),
            "see https://example.com/file.zip now"
        );
        assert_eq!(redact_urls(""), "");
        assert_eq!(redact_urls("https://"), "https://");
    }

    #[test]
    fn the_report_describes_a_data_directory_and_never_leaks_a_query() {
        use crate::model::{FileEntry, FileId, PackageId};
        let dir = tempfile::tempdir().unwrap();
        let mut file =
            FileEntry::new(FileId::new(1), "https://h.example/f?sig=SECRET", "f.bin").unwrap();
        file.status = crate::model::FileStatus::Failed(crate::model::FileError::Other {
            detail: "GET https://h.example/f?sig=SECRET returned 500".to_string(),
        });
        let package = Package::new(
            PackageId::new(1),
            "pkg",
            dir.path().to_path_buf(),
            vec![file],
        )
        .unwrap();
        let manifests = dir.path().join("packages");
        std::fs::create_dir_all(&manifests).unwrap();
        crate::storage::manifest::save_package_manifest(
            &manifests.join("pkg-1.json"),
            &crate::storage::manifest::PackageManifest::new(package),
        )
        .unwrap();
        std::fs::write(manifests.join("pkg-2.json.corrupt"), b"x").unwrap();
        let logs = log_dir(dir.path());
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(
            logs.join("nuxd.2026-10-12.log"),
            "line one\nGET https://h.example/f?sig=SECRET\n",
        )
        .unwrap();

        let report = from_disk(dir.path());
        assert!(report.contains("version:"), "{report}");
        assert!(report.contains("\"pkg\""), "{report}");
        assert!(
            report.contains("unreadable manifest: pkg-2.json.corrupt"),
            "{report}"
        );
        assert!(report.contains("recent log:"), "{report}");
        assert!(report.contains("returned 500"), "{report}");
        assert!(!report.contains("SECRET"), "{report}");
    }
}
