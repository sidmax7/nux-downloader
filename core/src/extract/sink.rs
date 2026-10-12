//! Where extracted entries land, and the checks that stand between an
//! untrusted archive and the filesystem (`ARCHITECTURE.md` §12.4,
//! `IMPLEMENTATION_PLAN.md` P5.4):
//!
//! - **Path traversal**: `..`, absolute paths, drive prefixes and UNC
//!   paths are refused (backslashes count as separators on every OS).
//! - **Symlinks** are never created; the backend reports them as skipped.
//! - **Bombs**: total bytes, bytes relative to the archive's own size,
//!   and entry count are capped (`Limits`).
//! - **Names**: on Windows, characters and device names it can't hold are
//!   replaced (`sanitize_component`, testable on any OS).

use crate::error::ExtractError;
use std::collections::HashSet;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_bytes: u64,
    /// Output may be at most this many times the archive's size...
    pub max_ratio: u64,
    /// ...but only once more than this much has been written, so small
    /// highly-compressible archives aren't refused.
    pub ratio_floor: u64,
    pub max_entries: u64,
}

impl Limits {
    #[must_use]
    pub fn from_config(config: &crate::config::EngineConfig) -> Self {
        Self {
            max_bytes: config.max_extract_bytes,
            max_ratio: config.max_extract_ratio,
            ratio_floor: 1 << 30,
            max_entries: config.max_extract_entries,
        }
    }
}

/// Splits an entry name into safe path components, or refuses it.
pub fn entry_components(name: &str, windows: bool) -> Result<Vec<String>, ExtractError> {
    let unsafe_path = || ExtractError::UnsafePath {
        entry: name.to_string(),
    };
    if name.contains('\0') {
        return Err(unsafe_path());
    }
    let normalized = name.replace('\\', "/");
    // A leading separator is an absolute (or UNC) path.
    if normalized.starts_with('/') {
        return Err(unsafe_path());
    }
    let mut out = Vec::new();
    for (index, part) in normalized.split('/').enumerate() {
        match part {
            "" | "." => {}
            ".." => return Err(unsafe_path()),
            _ => {
                // `C:` / `C:foo`: a drive prefix, on any OS's archive.
                let bytes = part.as_bytes();
                if index == 0
                    && bytes.len() >= 2
                    && bytes[0].is_ascii_alphabetic()
                    && bytes[1] == b':'
                {
                    return Err(unsafe_path());
                }
                out.push(sanitize_component(part, windows));
            }
        }
    }
    Ok(out)
}

/// Makes one path component safe to create. With `windows` set,
/// characters Windows forbids become `_`, trailing dots and spaces are
/// dropped and reserved device names get a `_` prefix. Without it, only
/// control characters are replaced.
#[must_use]
pub fn sanitize_component(part: &str, windows: bool) -> String {
    let mut s: String = part
        .chars()
        .map(|c| {
            let forbidden =
                c.is_control() || (windows && matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*'));
            if forbidden { '_' } else { c }
        })
        .collect();
    if windows {
        s = s.trim_end_matches(['.', ' ']).to_string();
        let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
        let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.as_bytes()[3].is_ascii_digit()
                && stem.as_bytes()[3] != b'0');
        if reserved {
            s.insert(0, '_');
        }
    }
    if s.is_empty() {
        s.push('_');
    }
    s
}

/// A progress report: bytes written so far, and the archive's total
/// uncompressed size when its headers declare one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Progress {
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub entries: u64,
}

pub struct Sink<'a> {
    root: PathBuf,
    canonical_root: PathBuf,
    limits: Limits,
    archive_size: u64,
    windows: bool,
    cancel: &'a AtomicBool,
    progress: &'a mut dyn FnMut(Progress),
    made_dirs: HashSet<PathBuf>,
    pub written: u64,
    pub entries: u64,
    pub skipped_links: u64,
    pub total_hint: u64,
}

impl<'a> Sink<'a> {
    pub fn new(
        root: &Path,
        limits: Limits,
        archive_size: u64,
        cancel: &'a AtomicBool,
        progress: &'a mut dyn FnMut(Progress),
    ) -> Result<Self, ExtractError> {
        fs::create_dir_all(root).map_err(|e| io("create", root, e))?;
        let canonical_root = root.canonicalize().map_err(|e| io("resolve", root, e))?;
        Ok(Self {
            root: root.to_path_buf(),
            canonical_root,
            limits,
            archive_size,
            windows: cfg!(windows),
            cancel,
            progress,
            made_dirs: HashSet::new(),
            written: 0,
            entries: 0,
            skipped_links: 0,
            total_hint: 0,
        })
    }

    #[cfg(test)]
    pub fn as_windows(mut self) -> Self {
        self.windows = true;
        self
    }

    pub fn check_cancel(&self) -> Result<(), ExtractError> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(ExtractError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn count_entry(&mut self) -> Result<(), ExtractError> {
        self.check_cancel()?;
        self.entries += 1;
        if self.entries > self.limits.max_entries {
            return Err(ExtractError::TooLarge {
                detail: format!("more than {} entries", self.limits.max_entries),
            });
        }
        Ok(())
    }

    /// Refuses an archive that declares more than the limits allow
    /// before anything is written.
    pub fn declare(&self, uncompressed: u64) -> Result<(), ExtractError> {
        if uncompressed > self.limits.max_bytes {
            return Err(ExtractError::TooLarge {
                detail: format!(
                    "{uncompressed} bytes uncompressed exceeds the {} byte limit",
                    self.limits.max_bytes
                ),
            });
        }
        Ok(())
    }

    /// A symlink (or other non-regular entry) is not created.
    pub fn skip_link(&mut self, name: &str) -> Result<(), ExtractError> {
        // Still validate the name: a hostile path is an error either way.
        entry_components(name, self.windows)?;
        self.count_entry()?;
        self.skipped_links += 1;
        Ok(())
    }

    pub fn dir(&mut self, name: &str) -> Result<(), ExtractError> {
        self.count_entry()?;
        let components = entry_components(name, self.windows)?;
        let mut path = self.root.clone();
        path.extend(&components);
        self.ensure_dir(&path)
    }

    /// The path an entry would be written to, with its parents created
    /// and checked to lie inside the output directory. Backends that
    /// write files themselves (RAR) use this.
    pub fn file_path(&mut self, name: &str) -> Result<PathBuf, ExtractError> {
        self.count_entry()?;
        let components = entry_components(name, self.windows)?;
        if components.is_empty() {
            return Err(ExtractError::UnsafePath {
                entry: name.to_string(),
            });
        }
        let mut path = self.root.clone();
        path.extend(&components);
        if let Some(parent) = path.parent() {
            self.ensure_dir(parent)?;
        }
        Ok(path)
    }

    fn ensure_dir(&mut self, dir: &Path) -> Result<(), ExtractError> {
        if self.made_dirs.contains(dir) {
            return Ok(());
        }
        fs::create_dir_all(dir).map_err(|e| io("create", dir, e))?;
        // Belt and braces: whatever exists on disk, the directory must
        // resolve to somewhere inside the output root.
        let resolved = dir.canonicalize().map_err(|e| io("resolve", dir, e))?;
        if !resolved.starts_with(&self.canonical_root) {
            return Err(ExtractError::UnsafePath {
                entry: dir.display().to_string(),
            });
        }
        self.made_dirs.insert(dir.to_path_buf());
        Ok(())
    }

    /// Writes one regular file from `reader`, enforcing the limits as
    /// bytes arrive (a header's declared size is not trusted).
    pub fn file(
        &mut self,
        name: &str,
        reader: &mut dyn Read,
        declared: u64,
    ) -> Result<(), ExtractError> {
        self.declare(declared)?;
        let path = self.file_path(name)?;
        let mut out = fs::File::create(&path).map_err(|e| io("create", &path, e))?;
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            self.check_cancel()?;
            let n = reader.read(&mut buffer).map_err(|e| {
                // Decoders report CRC and truncation as `InvalidData` /
                // `UnexpectedEof`.
                match e.kind() {
                    std::io::ErrorKind::InvalidData | std::io::ErrorKind::UnexpectedEof => {
                        ExtractError::Corrupt {
                            detail: format!("{name}: {e}"),
                        }
                    }
                    _ => io("read", &path, e),
                }
            })?;
            if n == 0 {
                break;
            }
            self.account(n as u64)?;
            out.write_all(&buffer[..n])
                .map_err(|e| io("write", &path, e))?;
            (self.progress)(Progress {
                bytes_done: self.written,
                bytes_total: self.total_hint,
                entries: self.entries,
            });
        }
        out.flush().map_err(|e| io("write", &path, e))?;
        Ok(())
    }

    /// Counts bytes against the limits.
    pub fn account(&mut self, n: u64) -> Result<(), ExtractError> {
        self.written += n;
        if self.written > self.limits.max_bytes {
            return Err(ExtractError::TooLarge {
                detail: format!("more than {} bytes would be written", self.limits.max_bytes),
            });
        }
        if self.written > self.limits.ratio_floor
            && self.written
                > self
                    .limits
                    .max_ratio
                    .saturating_mul(self.archive_size.max(1))
        {
            return Err(ExtractError::TooLarge {
                detail: format!(
                    "expands more than {}x its own size (an archive bomb?)",
                    self.limits.max_ratio
                ),
            });
        }
        Ok(())
    }
}

pub fn io(operation: &'static str, path: &Path, source: std::io::Error) -> ExtractError {
    ExtractError::Io {
        operation,
        path: path.display().to_string(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comps(name: &str) -> Result<Vec<String>, ExtractError> {
        entry_components(name, false)
    }

    #[test]
    fn plain_and_nested_names_pass() {
        assert_eq!(comps("a.txt").unwrap(), vec!["a.txt"]);
        assert_eq!(comps("dir/sub/b.txt").unwrap(), vec!["dir", "sub", "b.txt"]);
        assert_eq!(comps("./x//y/").unwrap(), vec!["x", "y"]);
    }

    #[test]
    fn traversal_and_absolute_paths_are_refused() {
        for bad in [
            "../evil",
            "a/../../evil",
            "a/..",
            "/etc/passwd",
            "\\windows\\system32",
            "..\\evil",
            "a\\..\\..\\evil",
            "C:\\evil.txt",
            "c:evil.txt",
            "//server/share/x",
            "\\\\server\\share\\x",
            "nul\0byte",
        ] {
            assert!(
                matches!(comps(bad), Err(ExtractError::UnsafePath { .. })),
                "{bad:?} was accepted"
            );
        }
    }

    #[test]
    fn a_colon_later_in_the_path_is_not_a_drive() {
        assert!(comps("dir/c:file").is_ok());
    }

    #[test]
    fn windows_names_are_sanitized() {
        let s = |n: &str| sanitize_component(n, true);
        assert_eq!(s("a<b>c:d.txt"), "a_b_c_d.txt");
        assert_eq!(s("name. . "), "name");
        assert_eq!(s("CON"), "_CON");
        assert_eq!(s("con.txt"), "_con.txt");
        assert_eq!(s("LPT1"), "_LPT1");
        assert_eq!(s("COM0"), "COM0");
        assert_eq!(s("file:stream"), "file_stream");
        assert_eq!(s("..."), "_");
        // Unix keeps what it can hold.
        assert_eq!(sanitize_component("a:b?c", false), "a:b?c");
        assert_eq!(sanitize_component("a\u{7}b", false), "a_b");
    }

    fn limits() -> Limits {
        Limits {
            max_bytes: 1000,
            max_ratio: 10,
            ratio_floor: 100,
            max_entries: 3,
        }
    }

    #[test]
    fn limits_are_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(false);
        let mut noop = |_: Progress| {};
        let mut sink = Sink::new(dir.path(), limits(), 5, &cancel, &mut noop).unwrap();
        // Ratio: 5-byte archive, floor 100, ratio 10 -> 50 allowed, but the
        // floor wins until 100 bytes are written.
        sink.file("a", &mut std::io::Cursor::new(vec![0u8; 100]), 100)
            .unwrap();
        // Past the floor and past ratio*size.
        assert!(matches!(
            sink.file("b", &mut std::io::Cursor::new(vec![0u8; 10]), 10),
            Err(ExtractError::TooLarge { .. })
        ));
    }

    #[test]
    fn total_size_and_entry_caps() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(false);
        let mut noop = |_: Progress| {};
        let mut sink = Sink::new(dir.path(), limits(), 1 << 20, &cancel, &mut noop).unwrap();
        assert!(matches!(
            sink.declare(5000),
            Err(ExtractError::TooLarge { .. })
        ));
        sink.dir("d1").unwrap();
        sink.dir("d2").unwrap();
        sink.dir("d3").unwrap();
        assert!(matches!(sink.dir("d4"), Err(ExtractError::TooLarge { .. })));
    }

    #[test]
    fn files_land_inside_the_root_and_cancel_works() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(false);
        let mut noop = |_: Progress| {};
        let mut sink = Sink::new(dir.path(), limits(), 1 << 20, &cancel, &mut noop).unwrap();
        sink.file("sub/f.txt", &mut std::io::Cursor::new(b"hi".to_vec()), 2)
            .unwrap();
        assert_eq!(std::fs::read(dir.path().join("sub/f.txt")).unwrap(), b"hi");
        assert!(
            sink.file("../x", &mut std::io::Cursor::new(Vec::new()), 0)
                .is_err()
        );
        cancel.store(true, Ordering::Relaxed);
        assert!(matches!(
            sink.file("late", &mut std::io::Cursor::new(vec![1u8]), 1),
            Err(ExtractError::Cancelled)
        ));
    }
}
