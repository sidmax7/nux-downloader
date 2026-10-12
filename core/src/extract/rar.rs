//! RAR backend (`unrar`, behind the `rar` feature). The UnRAR library
//! follows a multi-volume set from its first volume by itself, and
//! extracts to paths we choose, so every destination still comes from
//! the safe `Sink`.

use super::sink::Sink;
use crate::error::ExtractError;
use std::path::Path;
use unrar::Archive;
use unrar::error::{Code, UnrarError};

/// Unix mode type bits, as RAR stores them in `file_attr` for entries
/// made on Unix.
const S_IFMT: u32 = 0o170_000;
const S_IFLNK: u32 = 0o120_000;

pub fn extract(first_volume: &Path, sink: &mut Sink<'_>) -> Result<(), ExtractError> {
    let mut archive = Archive::new(first_volume)
        .open_for_processing()
        .map_err(map_error)?;
    while let Some(header) = archive.read_header().map_err(map_error)? {
        sink.check_cancel()?;
        let entry = header.entry();
        let name = entry.filename.to_string_lossy().into_owned();
        if entry.is_encrypted() {
            return Err(ExtractError::Unsupported {
                detail: format!("{name} is encrypted; encrypted archives aren't supported"),
            });
        }
        if entry.file_attr & S_IFMT == S_IFLNK {
            sink.skip_link(&name)?;
            archive = header.skip().map_err(map_error)?;
        } else if entry.is_directory() {
            sink.dir(&name)?;
            archive = header.skip().map_err(map_error)?;
        } else {
            let size = entry.unpacked_size;
            sink.declare(size)?;
            let path = sink.file_path(&name)?;
            archive = header.extract_to(&path).map_err(map_error)?;
            // The library may have created a link regardless of the
            // attributes we looked at; never leave one behind.
            if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
                let _ = std::fs::remove_file(&path);
                sink.skipped_links += 1;
            } else {
                sink.account(size)?;
            }
        }
    }
    Ok(())
}

fn map_error(error: UnrarError) -> ExtractError {
    match error.code {
        Code::MissingPassword | Code::BadPassword => ExtractError::Unsupported {
            detail: "encrypted RAR archives aren't supported".to_string(),
        },
        _ => ExtractError::Corrupt {
            detail: error.to_string(),
        },
    }
}
