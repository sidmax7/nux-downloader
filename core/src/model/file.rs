//! `FileEntry` and the types around it (`ARCHITECTURE.md` §4).

use super::{FileId, LinkId};
use serde::{Deserialize, Serialize};
use std::fmt;
use url::Url;

/// What a server said about which version of a file it's serving —
/// compared on resume and mid-download (`version::compare`, §10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileVersion {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// `0` means unknown (matches `FileEntry::expected_size == None`, but
    /// this is the version *as observed on one response*, so `0` is more
    /// natural here than another `Option`).
    pub total_bytes: u64,
}

/// How a file is split into blocks — depends on file size only
/// (`ARCHITECTURE.md` §7.4), computed once by `plan::plan_layout` and
/// never recomputed afterwards.
///
/// `count == 1 && block_size == u64::MAX` is the sentinel for "a single
/// open-ended block" (the server never reported a size): there is no
/// meaningful fixed size for that block, the same way an HTTP
/// `Range: bytes=0-` request has no upper bound. `u64::MAX` rather than
/// `0`, because `0` is a legitimate known size — an empty file is a
/// single *bounded* block of zero bytes (§5: a `416` on `bytes=0-0`
/// with `bytes */0` is a valid empty download, not an error), and that
/// must stay distinguishable from "unknown". Every other combination
/// gives the size of every block but the last, which holds the
/// remainder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockLayout {
    pub block_size: u64,
    pub count: u32,
}

impl BlockLayout {
    #[must_use]
    pub const fn is_single_open_ended(&self) -> bool {
        self.count == 1 && self.block_size == u64::MAX
    }
}

/// One block's progress. Belongs to a `FileEntry`, indexed by position in
/// `FileEntry::parts` (index == block index).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartStatus {
    /// Contiguous bytes secured from the block's start. Never exceeds the
    /// block's length; a persisted value never exceeds bytes that were
    /// `fsync`ed (§9.3) — that invariant is enforced by the driver, not by
    /// this type.
    pub durable: u64,
    /// The link that delivered most of this block, for the UI grid.
    pub link: Option<LinkId>,
}

impl PartStatus {
    #[must_use]
    pub const fn not_started() -> Self {
        Self {
            durable: 0,
            link: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileStatus {
    Unprobed,
    Queued,
    Preparing,
    Active,
    Complete,
    Verified,
    Failed(FileError),
}

/// Why a file failed. Kept separate from the operational `thiserror`
/// types in `error.rs`: this is a serializable *summary*, persisted in
/// the manifest, not the original error value (which may hold things
/// that don't round-trip through JSON, like an I/O error's OS code).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileError {
    /// The remote file changed mid-download or on resume (§10). The user
    /// can restart the file; other files in the package are unaffected.
    RemoteChanged {
        detail: String,
    },
    /// A block exceeded its retry budget on every available link.
    TooManyRetries,
    /// The initial probe failed.
    ProbeFailed {
        detail: String,
    },
    /// Downloaded bytes didn't validate (size or checksum mismatch, §12.3).
    VerifyFailed {
        detail: String,
    },
    /// Disk or filesystem error.
    Io {
        detail: String,
    },
    Other {
        detail: String,
    },
}

impl fmt::Display for FileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RemoteChanged { detail } => write!(f, "the file on the server changed: {detail}"),
            Self::TooManyRetries => write!(f, "too many failed attempts"),
            Self::ProbeFailed { detail } => write!(f, "couldn't reach the file: {detail}"),
            Self::VerifyFailed { detail } => {
                write!(f, "the downloaded file didn't verify: {detail}")
            }
            Self::Io { detail } => write!(f, "disk error: {detail}"),
            Self::Other { detail } => write!(f, "{detail}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub id: FileId,
    /// What the user or a `LinkResolver` gave us, before probing.
    pub source: String,
    /// The final URL after redirects, once probed.
    pub url: Option<Url>,
    pub name: String,
    pub expected_size: Option<u64>,
    pub supports_ranges: bool,
    /// Accepted validators; `[0]` is the version this file started on.
    pub versions: Vec<FileVersion>,
    pub layout: Option<BlockLayout>,
    /// One per block once `layout` is set; empty until then.
    pub parts: Vec<PartStatus>,
    pub status: FileStatus,
}

impl FileEntry {
    /// A freshly collected file, not yet probed. Returns `None` for an
    /// empty `source` or `name` — those are always a bug upstream, not a
    /// runtime condition to carry through the rest of the pipeline.
    #[must_use]
    pub fn new(id: FileId, source: impl Into<String>, name: impl Into<String>) -> Option<Self> {
        let source = source.into();
        let name = name.into();
        if source.is_empty() || name.is_empty() {
            return None;
        }
        Some(Self {
            id,
            source,
            url: None,
            name,
            expected_size: None,
            supports_ranges: false,
            versions: Vec::new(),
            layout: None,
            parts: Vec::new(),
            status: FileStatus::Unprobed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_rejects_empty_source() {
        assert!(FileEntry::new(FileId::new(1), "", "file.bin").is_none());
    }

    #[test]
    fn new_rejects_empty_name() {
        assert!(FileEntry::new(FileId::new(1), "https://example.com/f", "").is_none());
    }

    #[test]
    fn new_starts_unprobed() {
        let file = FileEntry::new(FileId::new(1), "https://example.com/f", "f.bin").unwrap();
        assert_eq!(file.status, FileStatus::Unprobed);
        assert!(file.parts.is_empty());
    }

    #[test]
    fn single_open_ended_sentinel() {
        let layout = BlockLayout {
            block_size: u64::MAX,
            count: 1,
        };
        assert!(layout.is_single_open_ended());
        // A known, bounded size — including the empty-file case — must
        // never be mistaken for "unknown".
        let layout = BlockLayout {
            block_size: 4096,
            count: 1,
        };
        assert!(!layout.is_single_open_ended());
        let layout = BlockLayout {
            block_size: 0,
            count: 1,
        };
        assert!(!layout.is_single_open_ended());
    }

    #[test]
    fn part_not_started_has_no_durable_bytes() {
        let part = PartStatus::not_started();
        assert_eq!(part.durable, 0);
        assert_eq!(part.link, None);
    }

    #[test]
    fn file_entry_serde_round_trip() {
        let mut file = FileEntry::new(FileId::new(7), "https://example.com/f", "f.bin").unwrap();
        file.expected_size = Some(1024);
        file.url = Some(Url::parse("https://example.com/f").unwrap());
        file.status = FileStatus::Failed(FileError::RemoteChanged {
            detail: "size changed".into(),
        });
        let json = serde_json::to_string(&file).unwrap();
        let back: FileEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, file);
    }

    #[test]
    fn file_error_display_is_human_readable() {
        let err = FileError::TooManyRetries;
        assert_eq!(err.to_string(), "too many failed attempts");
    }
}
