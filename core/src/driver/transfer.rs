//! One transfer attempt (`ARCHITECTURE.md` §8): request a range, validate
//! the response before writing a byte, stream the body with a per-read
//! stall timeout, and write it in place. `position` only advances after a
//! positional write has returned, which is what lets a checkpoint claim
//! every sampled position as durable once it has synced (§9.3).

use crate::engine::AttemptOutcome;
use crate::error::TransferError;
use crate::model::{ByteRange, FileVersion};
use crate::probe::parse_content_range;
use crate::storage::file::write_at;
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
    /// The version the download started on (§10), if the driver has it.
    pub accepted: Option<FileVersion>,
    pub stall_timeout: Duration,
    pub buffer_bytes: usize,
    /// Absolute file offset of the next byte this attempt will write.
    pub position: Arc<AtomicU64>,
    pub cancel: CancellationToken,
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
fn validate(response: &Response, spec: &AttemptSpec) -> Result<bool, TransferError> {
    let status = response.status();
    let start = spec.range.start;
    let (partial, total) = if status == StatusCode::PARTIAL_CONTENT {
        let served = header(response, CONTENT_RANGE)
            .and_then(parse_content_range)
            .ok_or(TransferError::MissingContentRange)?;
        if served.start != start || spec.range.end.is_some_and(|end| served.end > end) {
            return Err(TransferError::WrongRange);
        }
        (true, served.total)
    } else if status == StatusCode::OK && start == 0 {
        let length = header(response, CONTENT_LENGTH).and_then(|value| value.parse().ok());
        (false, length)
    } else {
        return Err(TransferError::Status(status.as_u16()));
    };

    if let Some(accepted) = &spec.accepted {
        let seen = FileVersion {
            etag: header(response, ETAG).map(str::to_string),
            last_modified: header(response, LAST_MODIFIED).map(str::to_string),
            total_bytes: total.unwrap_or(0),
        };
        // A size change is proof (§10). A validator-only mismatch needs
        // byte sampling to settle, which is P4.6; until then it's
        // accepted rather than failing a file a load balancer merely
        // relabelled.
        if let VersionCheck::Size { detail } = compare(std::slice::from_ref(accepted), &seen) {
            return Err(TransferError::RemoteChanged(detail));
        }
    }
    Ok(partial)
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
    let partial = validate(&response, spec)?;

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
