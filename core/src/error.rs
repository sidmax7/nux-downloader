//! Error taxonomy (`ARCHITECTURE.md` §16): one `thiserror` enum per
//! module, wrapped by `EngineError` at the public API boundary. Also
//! `Blame` and the pure function that assigns it — the only logic in
//! this module the engine actually consults at runtime. The rest is
//! scaffolding: future phases fill in each module's error variants as
//! they build the code that can actually produce them, so their shapes
//! aren't guessed at ahead of the code they describe.

use thiserror::Error;

/// Interface discovery and per-link binding (`net`, Phase 3).
#[derive(Debug, Error)]
pub enum NetError {}

/// The I/O half of probing (Phase 2, P2.4) — the pure parsing in
/// `probe.rs` doesn't need this; it just returns `Option`.
#[derive(Debug, Error)]
pub enum ProbeError {}

/// One transfer attempt's failure modes (`driver::transfer`, Phase 2).
/// Fully specified already, unlike its neighbors here: `blame` (below)
/// needs something concrete to match on, and it mirrors the §16 table
/// exactly.
#[derive(Debug, Error)]
pub enum TransferError {
    /// Connect failed, or timed out before the first response byte.
    #[error("couldn't connect: {0}")]
    Connect(String),
    /// The connection stalled mid-body (no bytes for the stall timeout).
    #[error("connection stalled")]
    Stall,
    /// The connection was reset before the body finished.
    #[error("connection reset")]
    Reset,
    /// The body ended before the range's full length arrived.
    #[error("short body: got {got} of {expected} bytes")]
    ShortBody { got: u64, expected: u64 },
    /// A `206`'s `Content-Range` didn't start where the request asked.
    #[error("server served the wrong range")]
    WrongRange,
    /// The body ran past the end of the requested range.
    #[error("server sent more than the requested range")]
    Overlong,
    /// A `206` with no usable `Content-Range` header.
    #[error("206 response missing a usable Content-Range")]
    MissingContentRange,
    /// Any HTTP status not covered by a more specific variant above.
    #[error("server responded {0}")]
    Status(u16),
    /// The remote file's version no longer matches what this download
    /// accepted (§10) — a size change, or an unresolvable validator
    /// mismatch.
    #[error("the file on the server changed: {0}")]
    RemoteChanged(String),
    /// A disk or filesystem error while writing.
    #[error("disk error: {0}")]
    Io(String),
}

/// `storage` errors. The manifest load/save variants here are used
/// starting in Phase 1 (P1.7); positional-write and preallocation
/// variants are added when that I/O is built (Phase 2, P2.2).
#[derive(Debug, Error)]
pub enum StorageError {
    #[error("couldn't read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("couldn't write {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is not valid JSON: {source}")]
    InvalidJson {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "{path} has schema version {found}, which is newer than this build supports (up to {supported})"
    )]
    UnsupportedSchemaVersion {
        path: String,
        found: u32,
        supported: u32,
    },
}

/// Extraction, if it ships (Phase 5).
#[derive(Debug, Error)]
pub enum ExtractError {}

/// The `LinkResolver` registry and `DirectUrl` (`resolve`, Phase 4).
#[derive(Debug, Error)]
pub enum ResolveError {}

/// The public API boundary's error type (`Engine`'s methods, §15).
#[derive(Debug, Error)]
pub enum EngineError {
    #[error(transparent)]
    Net(#[from] NetError),
    #[error(transparent)]
    Probe(#[from] ProbeError),
    #[error(transparent)]
    Transfer(#[from] TransferError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Extract(#[from] ExtractError),
    #[error(transparent)]
    Resolve(#[from] ResolveError),
}

/// Who a transfer failure counts against (§16) — decides whether a
/// link's health counter moves, a block simply retries, a whole file
/// fails, a host backs off, or a package pauses with an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blame {
    Link,
    Attempt,
    File,
    Host,
    Fatal,
}

/// The §16 table, as code.
///
/// `Status` codes not named in that table (anything besides
/// `416`/`404`/`403`/`410`/`429`/`503`) are treated like a malformed
/// range response: blamed on the attempt and retried. That degrades
/// gracefully — an odd status doesn't fail the whole file on one
/// occurrence — while a status that keeps recurring still exhausts the
/// block's retry budget and fails the file, the same path
/// `WrongRange`/`Overlong` take.
#[must_use]
pub fn blame(error: &TransferError) -> Blame {
    // Match arms are checked in order, so the specific `Status(...)`
    // patterns below claim their codes before the trailing `Status(_)`
    // catches everything else — no guard needed to keep them exclusive.
    match error {
        TransferError::Connect(_) => Blame::Link,

        TransferError::Stall | TransferError::Reset | TransferError::ShortBody { .. } => {
            Blame::Attempt
        }
        TransferError::WrongRange
        | TransferError::Overlong
        | TransferError::MissingContentRange => Blame::Attempt,

        TransferError::Status(416 | 404 | 403 | 410) => Blame::File,
        TransferError::Status(429 | 503) => Blame::Host,
        // Any status the table doesn't name.
        TransferError::Status(_) => Blame::Attempt,

        TransferError::RemoteChanged(_) => Blame::File,
        TransferError::Io(_) => Blame::Fatal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_failure_blames_the_link() {
        assert_eq!(
            blame(&TransferError::Connect("refused".into())),
            Blame::Link
        );
    }

    #[test]
    fn stall_reset_and_short_body_blame_the_attempt() {
        assert_eq!(blame(&TransferError::Stall), Blame::Attempt);
        assert_eq!(blame(&TransferError::Reset), Blame::Attempt);
        assert_eq!(
            blame(&TransferError::ShortBody {
                got: 1,
                expected: 2
            }),
            Blame::Attempt
        );
    }

    #[test]
    fn wrong_range_overlong_and_missing_content_range_blame_the_attempt() {
        assert_eq!(blame(&TransferError::WrongRange), Blame::Attempt);
        assert_eq!(blame(&TransferError::Overlong), Blame::Attempt);
        assert_eq!(blame(&TransferError::MissingContentRange), Blame::Attempt);
    }

    #[test]
    fn not_found_and_range_not_satisfiable_blame_the_file() {
        assert_eq!(blame(&TransferError::Status(416)), Blame::File);
        assert_eq!(blame(&TransferError::Status(404)), Blame::File);
    }

    #[test]
    fn forbidden_and_gone_blame_the_file() {
        assert_eq!(blame(&TransferError::Status(403)), Blame::File);
        assert_eq!(blame(&TransferError::Status(410)), Blame::File);
    }

    #[test]
    fn rate_limited_and_unavailable_blame_the_host() {
        assert_eq!(blame(&TransferError::Status(429)), Blame::Host);
        assert_eq!(blame(&TransferError::Status(503)), Blame::Host);
    }

    #[test]
    fn remote_changed_blames_the_file() {
        assert_eq!(
            blame(&TransferError::RemoteChanged("size differs".into())),
            Blame::File
        );
    }

    #[test]
    fn io_error_is_fatal() {
        assert_eq!(blame(&TransferError::Io("disk full".into())), Blame::Fatal);
    }

    #[test]
    fn an_unlisted_status_blames_the_attempt_rather_than_the_file() {
        assert_eq!(blame(&TransferError::Status(500)), Blame::Attempt);
        assert_eq!(blame(&TransferError::Status(401)), Blame::Attempt);
    }
}
