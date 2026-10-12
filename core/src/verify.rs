//! Package verification (`ARCHITECTURE.md` §12.3). Blocking: the driver
//! runs it on the blocking pool once every file of a package is on disk.
//!
//! - **Always**: each file exists and has exactly the expected length.
//! - **When a sidecar is in the package** (`.sfv` CRC32, `.md5`,
//!   `.sha256`): every file it lists that is also in the package is
//!   hashed and compared. Entries naming files that aren't in the
//!   package are ignored — a sidecar often covers a wider set.

use sha2::Digest;
use std::fmt;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

/// Sidecars bigger than this are not read; a checksum list is tiny.
const MAX_SIDECAR_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashKind {
    Crc32,
    Md5,
    Sha256,
}

impl fmt::Display for HashKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Crc32 => "CRC32",
            Self::Md5 => "MD5",
            Self::Sha256 => "SHA-256",
        })
    }
}

/// One checksum a sidecar asserts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expectation {
    /// The file name as the sidecar wrote it (any directory part removed).
    pub name: String,
    pub kind: HashKind,
    /// Lower-case hex.
    pub hash: String,
}

/// A file of the package under verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyFile {
    pub name: String,
    pub expected_size: Option<u64>,
}

/// What verification found wrong. `file` names the offending file when
/// there is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyFailure {
    pub file: Option<String>,
    /// `true` when the file's bytes are bad (wrong size or hash), so
    /// deleting and re-downloading it is the remedy; `false` for
    /// problems like an unreadable file.
    pub corrupt: bool,
    pub detail: String,
}

impl fmt::Display for VerifyFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.detail)
    }
}

/// What a successful verification covered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VerifyReport {
    pub sized: usize,
    pub hashed: usize,
}

/// Which sidecar format a file name denotes, if any.
#[must_use]
pub fn sidecar_kind(name: &str) -> Option<HashKind> {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".sfv") {
        Some(HashKind::Crc32)
    } else if lower.ends_with(".md5") || lower.ends_with(".md5sum") {
        Some(HashKind::Md5)
    } else if lower.ends_with(".sha256") || lower.ends_with(".sha256sum") {
        Some(HashKind::Sha256)
    } else {
        None
    }
}

/// Parses a sidecar's text. `.sfv` is `name CRC32`; `.md5`/`.sha256`
/// is `hash  name` (text mode) or `hash *name` (binary mode). Comment
/// lines (`;` or `#`) and anything unparseable are skipped.
#[must_use]
pub fn parse_sidecar(kind: HashKind, text: &str) -> Vec<Expectation> {
    let want_len = match kind {
        HashKind::Crc32 => 8,
        HashKind::Md5 => 32,
        HashKind::Sha256 => 64,
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        let (name, hash) = if kind == HashKind::Crc32 {
            // The CRC is the last whitespace-separated token; the name
            // may itself contain spaces.
            match line.rsplit_once(char::is_whitespace) {
                Some((name, hash)) => (name.trim(), hash),
                None => continue,
            }
        } else {
            match line.split_once(char::is_whitespace) {
                Some((hash, name)) => (name.trim_start().trim_start_matches('*'), hash),
                None => continue,
            }
        };
        // `dir/` and `..` name no file.
        let base = base_name(name);
        if hash.len() != want_len
            || !hash.bytes().all(|b| b.is_ascii_hexdigit())
            || matches!(base, "" | "." | "..")
        {
            continue;
        }
        out.push(Expectation {
            name: base.to_string(),
            kind,
            hash: hash.to_ascii_lowercase(),
        });
    }
    out
}

fn base_name(name: &str) -> &str {
    name.rsplit(['/', '\\']).next().unwrap_or(name)
}

/// Hex digest of `path`, checking `cancel` between reads.
pub fn hash_file(
    path: &Path,
    kind: HashKind,
    cancel: &AtomicBool,
) -> Result<String, VerifyFailure> {
    let io_error = |error: std::io::Error| VerifyFailure {
        file: path.file_name().map(|n| n.to_string_lossy().into_owned()),
        corrupt: false,
        detail: format!("couldn't read {}: {error}", path.display()),
    };
    let mut file = std::fs::File::open(path).map_err(io_error)?;
    let mut buffer = vec![0u8; 256 * 1024];
    enum Hasher {
        Crc32(crc32fast::Hasher),
        Md5(md5::Md5),
        Sha256(sha2::Sha256),
    }
    let mut hasher = match kind {
        HashKind::Crc32 => Hasher::Crc32(crc32fast::Hasher::new()),
        HashKind::Md5 => Hasher::Md5(md5::Md5::new()),
        HashKind::Sha256 => Hasher::Sha256(sha2::Sha256::new()),
    };
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(VerifyFailure {
                file: None,
                corrupt: false,
                detail: "cancelled".to_string(),
            });
        }
        let n = file.read(&mut buffer).map_err(io_error)?;
        if n == 0 {
            break;
        }
        match &mut hasher {
            Hasher::Crc32(h) => h.update(&buffer[..n]),
            Hasher::Md5(h) => h.update(&buffer[..n]),
            Hasher::Sha256(h) => h.update(&buffer[..n]),
        }
    }
    Ok(match hasher {
        Hasher::Crc32(h) => format!("{:08x}", h.finalize()),
        Hasher::Md5(h) => hex(&h.finalize()),
        Hasher::Sha256(h) => hex(&h.finalize()),
    })
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// Verifies the files of one package in `dir`.
pub fn verify_package(
    dir: &Path,
    files: &[VerifyFile],
    cancel: &AtomicBool,
) -> Result<VerifyReport, VerifyFailure> {
    let mut report = VerifyReport::default();

    for file in files {
        let path = dir.join(&file.name);
        let length = std::fs::metadata(&path)
            .map_err(|error| VerifyFailure {
                file: Some(file.name.clone()),
                // A file that is gone has to be fetched again.
                corrupt: error.kind() == std::io::ErrorKind::NotFound,
                detail: format!("{} is missing or unreadable: {error}", file.name),
            })?
            .len();
        if let Some(expected) = file.expected_size
            && length != expected
        {
            return Err(VerifyFailure {
                file: Some(file.name.clone()),
                corrupt: true,
                detail: format!("{} is {length} bytes, expected {expected}", file.name),
            });
        }
        report.sized += 1;
    }

    // Everything the package's sidecars assert, for files it contains.
    let mut checks: Vec<(String, Expectation)> = Vec::new();
    for sidecar in files {
        let Some(kind) = sidecar_kind(&sidecar.name) else {
            continue;
        };
        let path = dir.join(&sidecar.name);
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_SIDECAR_BYTES) {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        for expectation in parse_sidecar(kind, &text) {
            if let Some(target) = find_target(files, &expectation.name, &sidecar.name) {
                checks.push((target.to_string(), expectation));
            }
        }
    }
    for (target, expectation) in checks {
        let actual = hash_file(&dir.join(&target), expectation.kind, cancel)?;
        if actual != expectation.hash {
            return Err(VerifyFailure {
                file: Some(target.clone()),
                corrupt: true,
                detail: format!(
                    "{target}: {} is {actual}, expected {}",
                    expectation.kind, expectation.hash
                ),
            });
        }
        report.hashed += 1;
    }
    Ok(report)
}

/// The package file a sidecar entry refers to: exact name first, then
/// ignoring case. A sidecar never vouches for itself.
fn find_target<'a>(files: &'a [VerifyFile], name: &str, sidecar: &str) -> Option<&'a str> {
    let candidates = || {
        files
            .iter()
            .map(|f| f.name.as_str())
            .filter(|n| *n != sidecar)
    };
    candidates()
        .find(|n| *n == name)
        .or_else(|| candidates().find(|n| n.eq_ignore_ascii_case(name)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn crc(data: &[u8]) -> String {
        format!("{:08x}", crc32fast::hash(data))
    }

    fn sha(data: &[u8]) -> String {
        hex(&sha2::Sha256::digest(data))
    }

    fn md5(data: &[u8]) -> String {
        hex(&md5::Md5::digest(data))
    }

    fn vf(name: &str, size: Option<u64>) -> VerifyFile {
        VerifyFile {
            name: name.to_string(),
            expected_size: size,
        }
    }

    fn no_cancel() -> AtomicBool {
        AtomicBool::new(false)
    }

    #[test]
    fn parses_sfv_with_comments_and_spaces_in_names() {
        let text = "; generated\nfile one.bin DEADBEEF\r\nb.bin 0000ABCD\nnot a line\n";
        let parsed = parse_sidecar(HashKind::Crc32, text);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].name, "file one.bin");
        assert_eq!(parsed[0].hash, "deadbeef");
    }

    #[test]
    fn parses_md5_text_and_binary_modes_and_paths() {
        let h = "d41d8cd98f00b204e9800998ecf8427e";
        let text = format!("{h}  a.bin\n{h} *dir/b.bin\n# note\nshort  c.bin\n");
        let parsed = parse_sidecar(HashKind::Md5, &text);
        assert_eq!(
            parsed.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            vec!["a.bin", "b.bin"]
        );
    }

    #[test]
    fn sidecar_kinds() {
        assert_eq!(sidecar_kind("x.SFV"), Some(HashKind::Crc32));
        assert_eq!(sidecar_kind("x.md5"), Some(HashKind::Md5));
        assert_eq!(sidecar_kind("x.sha256"), Some(HashKind::Sha256));
        assert_eq!(sidecar_kind("x.bin"), None);
    }

    #[test]
    fn a_good_package_passes_every_check() {
        let dir = tempfile::tempdir().unwrap();
        let a = b"alpha bytes".to_vec();
        let b = b"beta bytes!".to_vec();
        std::fs::write(dir.path().join("a.bin"), &a).unwrap();
        std::fs::write(dir.path().join("b.bin"), &b).unwrap();
        std::fs::write(
            dir.path().join("sums.sfv"),
            format!("a.bin {}\nb.bin {}\n", crc(&a), crc(&b)),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("sums.sha256"),
            format!("{}  a.bin\nunrelated: {}  other.bin\n", sha(&a), sha(&b)),
        )
        .unwrap();
        let files = [
            vf("a.bin", Some(a.len() as u64)),
            vf("b.bin", Some(b.len() as u64)),
            vf("sums.sfv", None),
            vf("sums.sha256", None),
        ];
        let report = verify_package(dir.path(), &files, &no_cancel()).unwrap();
        assert_eq!(report.sized, 4);
        // 2 from the .sfv, 1 from the .sha256 (other.bin isn't ours).
        assert_eq!(report.hashed, 3);
    }

    #[test]
    fn md5_sidecar_is_checked() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.bin"), b"abc").unwrap();
        std::fs::write(dir.path().join("a.md5"), format!("{}  a.bin", md5(b"abc"))).unwrap();
        let files = [vf("a.bin", None), vf("a.md5", None)];
        assert_eq!(
            verify_package(dir.path(), &files, &no_cancel())
                .unwrap()
                .hashed,
            1
        );
    }

    #[test]
    fn a_corrupted_file_is_detected_and_named() {
        let dir = tempfile::tempdir().unwrap();
        let good = b"alpha bytes".to_vec();
        std::fs::write(dir.path().join("a.bin"), b"alpha bytez").unwrap();
        std::fs::write(
            dir.path().join("sums.sfv"),
            format!("a.bin {}\n", crc(&good)),
        )
        .unwrap();
        let files = [vf("a.bin", Some(11)), vf("sums.sfv", None)];
        let failure = verify_package(dir.path(), &files, &no_cancel()).unwrap_err();
        assert_eq!(failure.file.as_deref(), Some("a.bin"));
        assert!(failure.corrupt);
        assert!(failure.detail.contains("CRC32"), "{failure}");
    }

    #[test]
    fn a_wrong_length_is_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.bin"), b"short").unwrap();
        let failure =
            verify_package(dir.path(), &[vf("a.bin", Some(99))], &no_cancel()).unwrap_err();
        assert!(failure.corrupt);
        assert_eq!(failure.file.as_deref(), Some("a.bin"));
    }

    #[test]
    fn a_missing_file_has_to_be_fetched_again() {
        let dir = tempfile::tempdir().unwrap();
        let failure =
            verify_package(dir.path(), &[vf("gone.bin", None)], &no_cancel()).unwrap_err();
        assert!(failure.corrupt, "a vanished file must be refetched");
    }

    #[test]
    fn names_match_ignoring_case_and_a_sidecar_never_vouches_for_itself() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Movie.BIN"), b"x").unwrap();
        std::fs::write(
            dir.path().join("a.sfv"),
            format!("movie.bin {}\na.sfv 00000000\n", crc(b"x")),
        )
        .unwrap();
        let files = [vf("Movie.BIN", None), vf("a.sfv", None)];
        assert_eq!(
            verify_package(dir.path(), &files, &no_cancel())
                .unwrap()
                .hashed,
            1
        );
    }

    #[test]
    fn cancel_stops_hashing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.bin"), vec![1u8; 1000]).unwrap();
        let cancel = AtomicBool::new(true);
        let failure = hash_file(&dir.path().join("a.bin"), HashKind::Sha256, &cancel).unwrap_err();
        assert_eq!(failure.detail, "cancelled");
    }

    #[test]
    fn sidecar_entries_that_name_no_file_are_skipped() {
        // Found by fuzzing: `dir/` has an empty base name.
        let h = "d41d8cd98f00b204e9800998ecf8427e";
        let text = format!("{h}  dir/\n{h}  ..\n{h}  .\n{h}  real.bin\n");
        let parsed = parse_sidecar(HashKind::Md5, &text);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "real.bin");
    }

    #[test]
    fn known_digests() {
        assert_eq!(crc(b"123456789"), "cbf43926");
        assert_eq!(md5(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(
            sha(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
