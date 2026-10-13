//! `Package` and the types around it (`ARCHITECTURE.md` §4).

use super::{ByteRate, FileEntry, PackageId, Priority};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;

/// Set by multi-part archive grouping (§12.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveSet {
    pub scheme: ArchiveScheme,
    /// The shared basename every volume in the set groups under.
    pub base_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ArchiveScheme {
    Rar,
    RarLegacy,
    SevenZip,
    SplitZip,
    GenericSplit,
}

/// What to do with a package once it has downloaded and verified
/// (§12.4). `extract` and `delete_archives` are independent options:
/// the second applies only after a successful extraction.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PostOptions {
    pub archive: Option<ArchiveSet>,
    pub extract: bool,
    pub delete_archives: bool,
    /// The folder made for the package (Decision 37), if any; see
    /// `Package::folder`.
    pub folder: Option<PathBuf>,
}

/// Why a package failed — a serializable summary, same rationale as
/// `FileError` (see `file.rs`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PackageError {
    /// One or more files in the package failed.
    FilesFailed,
    VerifyFailed {
        detail: String,
    },
    ExtractFailed {
        detail: String,
    },
    /// Disk full or another fatal error (§16: "Fatal (package)").
    Io {
        detail: String,
    },
    Other {
        detail: String,
    },
}

impl fmt::Display for PackageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FilesFailed => write!(f, "one or more files failed"),
            Self::VerifyFailed { detail } => write!(f, "verification failed: {detail}"),
            Self::ExtractFailed { detail } => write!(f, "extraction failed: {detail}"),
            Self::Io { detail } if detail.starts_with("disk error") => write!(f, "{detail}"),
            Self::Io { detail } => write!(f, "disk error: {detail}"),
            Self::Other { detail } => write!(f, "{detail}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PackageStatus {
    Queued,
    Downloading,
    Paused,
    Verifying,
    Extracting,
    Done,
    Failed(PackageError),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Package {
    pub id: PackageId,
    pub name: String,
    #[serde(with = "super::path_serde")]
    pub dest_dir: PathBuf,
    pub priority: Priority,
    /// `len() == 1` is a simple download; `len() > 1` is a grouped set.
    /// Never empty — see `Package::new`.
    pub files: Vec<FileEntry>,
    pub archive: Option<ArchiveSet>,
    /// Extract the archive once it has downloaded and verified.
    #[serde(default)]
    pub extract: bool,
    /// Delete the archive files after a successful extraction.
    #[serde(default)]
    pub delete_archives: bool,
    /// The folder made for this package (Decision 37). Extraction puts
    /// its output here, and `dest_dir` is this folder or, for an archive
    /// being extracted, its `Archives` subfolder. `None` for a package
    /// added without its own folder (and every package saved before
    /// 0.2.1): `dest_dir` is then the whole story.
    #[serde(
        default,
        with = "super::path_serde::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub folder: Option<PathBuf>,
    pub limit: Option<ByteRate>,
    pub status: PackageStatus,
    /// Why the package is `Paused` against the user's wishes: a disk or
    /// I/O error (disk full, destination gone, permission denied). The
    /// partial data is intact; `Resume` clears this and tries again.
    #[serde(default)]
    pub last_error: Option<PackageError>,
}

impl Package {
    /// A freshly collected package, queued and with no archive grouping
    /// or rate limit yet. Returns `None` for an empty `name` or an empty
    /// `files` — a package with nothing to download can't exist.
    #[must_use]
    pub fn new(
        id: PackageId,
        name: impl Into<String>,
        dest_dir: PathBuf,
        files: Vec<FileEntry>,
    ) -> Option<Self> {
        let name = name.into();
        if name.is_empty() || files.is_empty() {
            return None;
        }
        Some(Self {
            id,
            name,
            dest_dir,
            priority: Priority::default(),
            files,
            archive: None,
            extract: false,
            delete_archives: false,
            folder: None,
            limit: None,
            status: PackageStatus::Queued,
            last_error: None,
        })
    }

    /// Whether every file has finished and validated — the precondition
    /// for `PackageStatus::Done` (§4 invariant).
    #[must_use]
    pub fn all_files_verified(&self) -> bool {
        self.files
            .iter()
            .all(|file| file.status == super::FileStatus::Verified)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FileId;

    fn one_file() -> Vec<FileEntry> {
        vec![FileEntry::new(FileId::new(1), "https://example.com/f", "f.bin").unwrap()]
    }

    #[test]
    fn new_rejects_empty_name() {
        assert!(Package::new(PackageId::new(1), "", PathBuf::from("/tmp"), one_file()).is_none());
    }

    #[test]
    fn new_rejects_no_files() {
        assert!(Package::new(PackageId::new(1), "pkg", PathBuf::from("/tmp"), vec![]).is_none());
    }

    #[test]
    fn new_starts_queued_with_normal_priority() {
        let pkg =
            Package::new(PackageId::new(1), "pkg", PathBuf::from("/tmp"), one_file()).unwrap();
        assert_eq!(pkg.status, PackageStatus::Queued);
        assert_eq!(pkg.priority, Priority::Normal);
        assert!(pkg.archive.is_none());
    }

    #[test]
    fn all_files_verified_false_when_any_file_is_not() {
        let pkg =
            Package::new(PackageId::new(1), "pkg", PathBuf::from("/tmp"), one_file()).unwrap();
        assert!(!pkg.all_files_verified());
    }

    #[test]
    fn all_files_verified_true_when_every_file_is() {
        let mut pkg =
            Package::new(PackageId::new(1), "pkg", PathBuf::from("/tmp"), one_file()).unwrap();
        pkg.files[0].status = super::super::FileStatus::Verified;
        assert!(pkg.all_files_verified());
    }

    #[test]
    fn package_serde_round_trip() {
        let mut pkg = Package::new(
            PackageId::new(3),
            "pkg",
            PathBuf::from("/tmp/dl"),
            one_file(),
        )
        .unwrap();
        pkg.status = PackageStatus::Failed(PackageError::FilesFailed);
        let json = serde_json::to_string(&pkg).unwrap();
        let back: Package = serde_json::from_str(&json).unwrap();
        assert_eq!(back, pkg);
    }
}
