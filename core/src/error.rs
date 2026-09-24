//! Error taxonomy (`ARCHITECTURE.md` §16): one `thiserror` enum per
//! module, wrapped by `EngineError` at the public API boundary. Also
//! `Blame` and the pure function that assigns it — the only logic in
//! this module the engine actually consults at runtime. The rest is
//! scaffolding: future phases fill in each module's error variants as
//! they build the code that can actually produce them, so their shapes
//! aren't guessed at ahead of the code they describe.

use thiserror::Error;

/// Interface discovery and per-link binding join this in Phase 3
/// (`net::discover`, `net::bind`); `BuildClient` is used starting in
/// Phase 2, P2.3.
#[derive(Debug, Error)]
pub enum NetError {
    /// `reqwest::ClientBuilder::build()` failed — a bad TLS
    /// configuration, or (on Linux/macOS, once `net::bind` is built)
    /// binding to an interface that no longer exists.
    #[error("couldn't build an HTTP client: {source}")]
    BuildClient {
        #[source]
        source: reqwest::Error,
    },
    /// A link has nothing to bind a connection to on this platform: no
    /// usable address for `BindMode::Address`.
    #[error("link {link} has no address to bind to")]
    NoBindAddress { link: String },
}

/// The I/O half of probing (Phase 2, P2.4) — the pure parsing in
/// `probe.rs` doesn't need this; it just returns `Option`.
#[derive(Debug, Error)]
pub enum ProbeError {
    /// Sending the ranged `GET` failed at the transport level (connect
    /// refused, TLS failure, redirect loop past the client's limit, …).
    #[error("probe request failed: {source}")]
    Request {
        #[source]
        source: reqwest::Error,
    },
    /// No response at all within `EngineConfig::stall_timeout` — a
    /// stalled connect, or a server that never sends headers.
    #[error("probe timed out")]
    TimedOut,
    /// A `206` response with no `Content-Range` header, one that doesn't
    /// parse, or one whose start doesn't match the `bytes=0-0` request.
    #[error("206 response had a malformed or unexpected Content-Range")]
    MalformedContentRange,
    /// Any status other than `200`, `206`, or a `416` reporting
    /// `bytes */0` (an empty file, §5).
    #[error("unexpected probe response status: {status}")]
    UnexpectedStatus { status: u16 },
    /// `429`/`503`: the host is busy or rate-limiting us, not refusing
    /// the file. `retry_after` is its `Retry-After`, if it sent one in
    /// seconds.
    #[error("server is busy ({status}){}", retry_after_note(*retry_after))]
    Busy {
        status: u16,
        retry_after: Option<std::time::Duration>,
    },
}

fn retry_after_note(retry_after: Option<std::time::Duration>) -> String {
    retry_after.map_or_else(String::new, |d| format!(", retry after {}s", d.as_secs()))
}

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
    /// `429`/`503`: the host is busy or rate-limiting this client. See
    /// `ProbeError::Busy`.
    #[error("server is busy ({status}){}", retry_after_note(*retry_after))]
    Busy {
        status: u16,
        retry_after: Option<std::time::Duration>,
    },
    /// The remote file's version no longer matches what this download
    /// accepted (§10) — a size change, or an unresolvable validator
    /// mismatch.
    #[error("the file on the server changed: {0}")]
    RemoteChanged(String),
    /// A disk or filesystem error while writing.
    #[error("disk error: {0}")]
    Io(String),
}

/// `storage` errors. The manifest load/save variants (`Read`, `Write`,
/// `InvalidJson`, `UnsupportedSchemaVersion`) are used starting in
/// Phase 1 (P1.7); `Io` and `NameReservationExhausted` are for the file
/// I/O built in Phase 2, P2.2.
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
    /// Any other file operation on a download's own files: create,
    /// preallocate, positional write, sync, rename, or a free-space
    /// check. `operation` is a short, fixed label (`"create"`,
    /// `"preallocate"`, …), not user input.
    #[error("couldn't {operation} {path}: {source}")]
    Io {
        operation: &'static str,
        path: String,
        #[source]
        source: std::io::Error,
    },
    /// `reserve_destination_path` tried every `name (n).ext` up to its
    /// limit and every one was already taken.
    #[error("couldn't find an unused file name for {name} in {dir}")]
    NameReservationExhausted { dir: String, name: String },
    /// The upfront free-space check before activation (§9.2) failed.
    #[error("not enough free space in {dir}: need {needed} bytes, {available} available")]
    InsufficientSpace {
        dir: String,
        needed: u64,
        available: u64,
    },
}

/// Extraction, if it ships (Phase 5).
#[derive(Debug, Error)]
pub enum ExtractError {}

/// The `LinkResolver` registry and `DirectUrl` (`resolve`, Phase 4).
#[derive(Debug, Error)]
pub enum ResolveError {
    /// Not something a URL can be made of.
    #[error("not a valid URL: {input}")]
    InvalidUrl { input: String },
    /// A URL, but not one this tool downloads (`http`/`https` only).
    #[error("unsupported scheme `{scheme}` in {input}")]
    UnsupportedScheme { input: String, scheme: String },
    /// No registered resolver claimed the input.
    #[error("nothing can resolve {input}")]
    NoResolver { input: String },
    /// A resolver claimed the input but couldn't produce links.
    #[error("couldn't resolve {input}: {detail}")]
    Failed { input: String, detail: String },
}

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
    /// A command named a package the engine doesn't have (never added, or
    /// already removed).
    #[error("no such package: {0}")]
    UnknownPackage(crate::model::PackageId),
    #[error("no such file: {0}")]
    UnknownFile(crate::model::FileId),
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
        TransferError::Status(429 | 503) | TransferError::Busy { .. } => Blame::Host,
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
    fn busy_blames_the_host() {
        let busy = TransferError::Busy {
            status: 429,
            retry_after: Some(std::time::Duration::from_secs(10)),
        };
        assert_eq!(blame(&busy), Blame::Host);
        assert_eq!(busy.to_string(), "server is busy (429), retry after 10s");
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
