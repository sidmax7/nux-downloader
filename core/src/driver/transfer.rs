//! One transfer attempt (`ARCHITECTURE.md` §8): request a range, validate
//! the response before writing a byte, stream the body with a per-read
//! stall timeout, and write it in place. `position` only advances after a
//! positional write has returned, which is what lets a checkpoint claim
//! every sampled position as durable once it has synced (§9.3).

use crate::engine::AttemptOutcome;
use crate::error::TransferError;
use crate::model::{ByteRange, FileVersion};
use crate::probe::parse_content_range;
use crate::storage::file::{read_at, write_at};
use crate::version::{VersionCheck, compare};
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, ETAG, LAST_MODIFIED, RANGE};
use reqwest::{Client, Response, StatusCode};
use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use url::Url;

/// Everything one attempt needs, owned, so it can run on its own task.
pub struct AttemptSpec {
    pub client: Client,
    pub url: Url,
    pub range: ByteRange,
    pub file: Arc<File>,
    pub path: PathBuf,
    /// The file's size as probed, if known — decides whether a request
    /// starting at byte 0 covers the whole file.
    pub expected_size: Option<u64>,
    /// Every version this download has accepted so far (§10) — the one it
    /// started on, plus any later relabelling confirmed by sampling.
    pub accepted: Vec<FileVersion>,
    /// Durable regions of the file to compare against fresh server bytes
    /// when a response's validators differ (§10). Empty when nothing is
    /// durable yet.
    pub samples: Vec<SampleRegion>,
    /// Set to the new version when this attempt confirmed by sampling
    /// that a differently labelled response holds identical bytes; the
    /// driver forwards it to the engine.
    pub confirmed_version: Arc<std::sync::Mutex<Option<FileVersion>>>,
    pub stall_timeout: Duration,
    pub buffer_bytes: usize,
    /// Speed limits this attempt must respect (§11).
    pub throttle: crate::limit::Throttle,
    /// Absolute file offset of the next byte this attempt will write.
    pub position: Arc<AtomicU64>,
    pub cancel: CancellationToken,
}

/// A stretch of already-durable bytes to compare with the server's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleRegion {
    pub offset: u64,
    pub len: u64,
}

/// Runs the attempt to completion or failure. `None` means it was
/// cancelled: the engine already forgot it (`AbortAttempt`), so there is
/// nothing to report.
pub async fn run(spec: AttemptSpec) -> Option<(AttemptOutcome, u64)> {
    let cancel = spec.cancel.clone();
    tokio::select! {
        biased;
        () = cancel.cancelled() => None,
        result = transfer(&spec) => {
            let outcome = match result {
                Ok(()) => AttemptOutcome::Success,
                Err(error) => AttemptOutcome::Failed(error),
            };
            Some((outcome, spec.position.load(Ordering::Acquire)))
        }
    }
}

fn header(response: &Response, name: reqwest::header::HeaderName) -> Option<&str> {
    response.headers().get(name)?.to_str().ok()
}

/// §8 step 1: a request starting at byte 0 that covers the whole file is
/// sent without `Range` — an empty or unknown-size file would answer
/// `bytes=0-` with `416`.
fn range_header(range: ByteRange, expected_size: Option<u64>) -> Option<String> {
    let whole_file = range.start == 0
        && match range.end {
            None => true,
            Some(end) => expected_size == end.checked_add(1),
        };
    if whole_file {
        return None;
    }
    Some(match range.end {
        Some(end) => format!("bytes={}-{end}", range.start),
        None => format!("bytes={}-", range.start),
    })
}

/// §8 step 2: validates status, `Content-Range` and version before any
/// byte is written. Returns whether the response is a `206`.
///
/// The second value is `Some(version)` when only the validators (ETag,
/// `Last-Modified`) differ from every accepted version: a hint, settled
/// by `confirm_by_sampling` before any byte is written. A size change is
/// proof and fails right here.
fn validate(
    response: &Response,
    spec: &AttemptSpec,
) -> Result<(bool, Option<FileVersion>), TransferError> {
    let status = response.status();
    let start = spec.range.start;
    let (partial, total) = if status == StatusCode::PARTIAL_CONTENT {
        let served = header(response, CONTENT_RANGE)
            .and_then(parse_content_range)
            .ok_or(TransferError::MissingContentRange)?;
        if served.start != start || spec.range.end.is_some_and(|end| served.end > end) {
            tracing::debug!(
                requested_start = start,
                requested_end = ?spec.range.end,
                served_start = served.start,
                served_end = served.end,
                content_range = header(response, CONTENT_RANGE),
                "server served a different range than requested"
            );
            return Err(TransferError::WrongRange);
        }
        (true, served.total)
    } else if status == StatusCode::OK && start == 0 {
        let length = header(response, CONTENT_LENGTH).and_then(|value| value.parse().ok());
        (false, length)
    } else if matches!(status.as_u16(), 429 | 503) {
        return Err(TransferError::Busy {
            status: status.as_u16(),
            retry_after: crate::probe::retry_after(response.headers()),
        });
    } else {
        return Err(TransferError::Status(status.as_u16()));
    };

    let mut needs_confirmation = None;
    if !spec.accepted.is_empty() {
        let seen = FileVersion {
            etag: header(response, ETAG).map(str::to_string),
            last_modified: header(response, LAST_MODIFIED).map(str::to_string),
            total_bytes: total.unwrap_or(0),
        };
        match compare(&spec.accepted, &seen) {
            VersionCheck::Same => {}
            // A size change is proof (§10).
            VersionCheck::Size { detail } => return Err(TransferError::RemoteChanged(detail)),
            VersionCheck::Validators { .. } => needs_confirmation = Some(seen),
        }
    }
    Ok((partial, needs_confirmation))
}

/// How many 16 KiB samples settle a validator mismatch (§10).
pub const MAX_SAMPLES: usize = 8;
/// Bytes per sample.
pub const SAMPLE_LEN: u64 = 16 * 1024;

/// §10: different ETag/`Last-Modified` with the same size *maybe* means a
/// different file — load balancers label identical bytes differently.
/// Settles it by re-fetching small samples of durable regions from the
/// server now presenting the new label and comparing them with what is
/// on disk. All identical → the new label is accepted; any difference →
/// `RemoteChanged`; a sample that can't be fetched → an ordinary
/// (retried) failure. With nothing durable yet there is nothing to be
/// stitched inconsistently, so the new label is simply adopted.
async fn confirm_by_sampling(spec: &AttemptSpec) -> Result<(), TransferError> {
    for region in spec.samples.iter().take(MAX_SAMPLES) {
        let end = region.offset + region.len - 1;
        let request = spec
            .client
            .get(spec.url.clone())
            .header(RANGE, format!("bytes={}-{end}", region.offset));
        let response = match timeout(spec.stall_timeout, request.send()).await {
            Err(_) => {
                return Err(TransferError::Connect(
                    "sample request timed out".to_string(),
                ));
            }
            Ok(Err(error)) => return Err(TransferError::Connect(error.to_string())),
            Ok(Ok(response)) => response,
        };
        let served = header(&response, CONTENT_RANGE).and_then(parse_content_range);
        if response.status() != StatusCode::PARTIAL_CONTENT
            || served.is_none_or(|range| range.start != region.offset)
        {
            return Err(TransferError::Connect(
                "sample request wasn't answered with the requested range".to_string(),
            ));
        }
        let fetched = match timeout(spec.stall_timeout, response.bytes()).await {
            Err(_) | Ok(Err(_)) => return Err(TransferError::Reset),
            Ok(Ok(bytes)) => bytes,
        };
        let mut local = vec![0u8; usize::try_from(region.len).unwrap_or(0)];
        let file = spec.file.clone();
        let path = spec.path.clone();
        let offset = region.offset;
        let local = tokio::task::spawn_blocking(move || {
            read_at(&file, &path, offset, &mut local).map(|()| local)
        })
        .await
        .map_err(|error| TransferError::Io(error.to_string()))?
        .map_err(|error| TransferError::Io(error.to_string()))?;
        if fetched.len() != local.len() || fetched[..] != local[..] {
            return Err(TransferError::RemoteChanged(format!(
                "the bytes at offset {} differ from what was already downloaded",
                region.offset
            )));
        }
    }
    Ok(())
}

/// Buffered, in-place writer for one attempt: bytes are written at their
/// final offset, and `position` is advanced only once `write_at`
/// has returned (§8 step 4).
struct Writer<'a> {
    spec: &'a AttemptSpec,
    buffer: Vec<u8>,
    offset: u64,
}

impl Writer<'_> {
    async fn flush(&mut self) -> Result<(), TransferError> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let data = std::mem::replace(&mut self.buffer, Vec::with_capacity(self.spec.buffer_bytes));
        let len = data.len() as u64;
        let file = self.spec.file.clone();
        let path = self.spec.path.clone();
        let at = self.offset;
        tokio::task::spawn_blocking(move || write_at(&file, &path, at, &data))
            .await
            .map_err(|error| TransferError::Io(error.to_string()))?
            .map_err(|error| TransferError::Io(error.to_string()))?;
        self.offset += len;
        self.spec.position.store(self.offset, Ordering::Release);
        Ok(())
    }

    async fn push(&mut self, data: &[u8]) -> Result<(), TransferError> {
        self.buffer.extend_from_slice(data);
        if self.buffer.len() >= self.spec.buffer_bytes {
            self.flush().await?;
        }
        Ok(())
    }

    /// Writes out whatever is buffered, then returns `error` — bytes that
    /// arrived before a failure are still valid and still count.
    async fn fail(&mut self, error: TransferError) -> Result<(), TransferError> {
        self.flush().await?;
        Err(error)
    }
}

async fn transfer(spec: &AttemptSpec) -> Result<(), TransferError> {
    let mut request = spec.client.get(spec.url.clone());
    if let Some(value) = range_header(spec.range, spec.expected_size) {
        request = request.header(RANGE, value);
    }
    let mut response = match timeout(spec.stall_timeout, request.send()).await {
        Err(_) => {
            return Err(TransferError::Connect(
                "timed out waiting for response headers".to_string(),
            ));
        }
        Ok(Err(error)) => return Err(TransferError::Connect(error.to_string())),
        Ok(Ok(response)) => response,
    };
    let (partial, unconfirmed) = validate(&response, spec)?;
    if let Some(version) = unconfirmed {
        confirm_by_sampling(spec).await?;
        if let Ok(mut slot) = spec.confirmed_version.lock() {
            *slot = Some(version);
        }
    }

    let wanted = spec.range.end.map(|end| end - spec.range.start + 1);
    let mut received = 0u64;
    let mut writer = Writer {
        spec,
        buffer: Vec::with_capacity(spec.buffer_bytes),
        offset: spec.range.start,
    };

    loop {
        let chunk = match timeout(spec.stall_timeout, response.chunk()).await {
            Err(_) => return writer.fail(TransferError::Stall).await,
            Ok(Err(_)) => return writer.fail(TransferError::Reset).await,
            Ok(Ok(None)) => break,
            Ok(Ok(Some(chunk))) => chunk,
        };

        // Wait for the limiters before taking the bytes in: the unread
        // rest of the response backs up into TCP, which is the throttle.
        spec.throttle.acquire(chunk.len()).await;

        let mut data: &[u8] = &chunk;
        let mut past_end = false;
        if let Some(wanted) = wanted {
            let room = usize::try_from(wanted - received).unwrap_or(usize::MAX);
            if data.len() > room {
                data = &data[..room];
                past_end = true;
            }
        }
        received += data.len() as u64;
        writer.push(data).await?;

        if past_end {
            writer.flush().await?;
            // A `206` promised exactly this range, so more is a protocol
            // error (§8 step 5). A `200` is the whole file from byte 0 —
            // a server that ignored `Range` — and everything this block
            // needs has arrived, so stopping here is success.
            return if partial {
                Err(TransferError::Overlong)
            } else {
                Ok(())
            };
        }
    }

    writer.flush().await?;
    match wanted {
        Some(wanted) if received < wanted => Err(TransferError::ShortBody {
            got: received,
            expected: wanted,
        }),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_whole_known_file_from_zero_sends_no_range() {
        let range = ByteRange::bounded(0, 99).unwrap();
        assert_eq!(range_header(range, Some(100)), None);
    }

    #[test]
    fn an_open_ended_request_from_zero_sends_no_range() {
        assert_eq!(range_header(ByteRange::open_ended(0), None), None);
    }

    #[test]
    fn a_first_block_of_a_larger_file_sends_a_range() {
        let range = ByteRange::bounded(0, 99).unwrap();
        assert_eq!(
            range_header(range, Some(1000)),
            Some("bytes=0-99".to_string())
        );
    }

    #[test]
    fn a_resumed_open_ended_request_sends_an_open_range() {
        assert_eq!(
            range_header(ByteRange::open_ended(500), None),
            Some("bytes=500-".to_string())
        );
    }
}
