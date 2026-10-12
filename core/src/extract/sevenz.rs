//! 7z backend (`sevenz-rust2`), writing through the safe `Sink`.

use super::sink::Sink;
use crate::error::ExtractError;
use sevenz_rust2::{ArchiveReader, Error as SevenError, Password};
use std::io::{BufReader, Read, Seek};

/// Unix-extension attribute bits: the mode lives in the high 16 bits.
const UNIX_EXTENSION: u32 = 0x8000;
const S_IFMT: u32 = 0o170_000;
const S_IFLNK: u32 = 0o120_000;

pub fn extract<R: Read + Seek>(reader: R, sink: &mut Sink<'_>) -> Result<(), ExtractError> {
    let mut archive =
        ArchiveReader::new(BufReader::new(reader), Password::empty()).map_err(map_error)?;
    let total: u64 = archive
        .archive()
        .files
        .iter()
        .filter(|f| f.has_stream())
        .map(sevenz_rust2::ArchiveEntry::size)
        .fold(0, u64::saturating_add);
    sink.declare(total)?;
    sink.total_hint = total;

    // The callback can only return the crate's own error type, so ours
    // is parked here and returned afterwards.
    let mut failure: Option<ExtractError> = None;
    let outcome = archive.for_each_entries(|entry, data| {
        let step = (|| {
            sink.check_cancel()?;
            if entry.is_anti_item() {
                return Ok(());
            }
            let attributes = entry.windows_attributes();
            let is_link =
                attributes & UNIX_EXTENSION != 0 && (attributes >> 16) & S_IFMT == S_IFLNK;
            if is_link {
                sink.skip_link(entry.name())
            } else if entry.is_directory() {
                sink.dir(entry.name())
            } else if entry.has_stream() {
                sink.file(entry.name(), data, entry.size())
            } else {
                sink.file(entry.name(), &mut std::io::empty(), 0)
            }
        })();
        match step {
            Ok(()) => Ok(true),
            Err(error) => {
                failure = Some(error);
                Err(SevenError::Other("extraction stopped".into()))
            }
        }
    });
    if let Some(error) = failure {
        return Err(error);
    }
    outcome.map_err(map_error)
}

fn map_error(error: SevenError) -> ExtractError {
    let text = error.to_string();
    let lower = text.to_ascii_lowercase();
    if lower.contains("password") || lower.contains("encrypt") {
        ExtractError::Unsupported {
            detail: format!("encrypted 7z archives aren't supported ({text})"),
        }
    } else {
        ExtractError::Corrupt { detail: text }
    }
}
