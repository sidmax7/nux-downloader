//! ZIP backend (the `zip` crate), writing through the safe `Sink`.

use super::sink::{Sink, io};
use crate::error::ExtractError;
use std::io::{BufReader, Read, Seek};
use std::path::Path;

pub fn extract<R: Read + Seek>(reader: R, sink: &mut Sink<'_>) -> Result<(), ExtractError> {
    let mut archive = ::zip::ZipArchive::new(BufReader::new(reader)).map_err(map_error)?;
    let mut total = 0u64;
    for index in 0..archive.len() {
        total = total.saturating_add(archive.by_index_raw(index).map_err(map_error)?.size());
    }
    sink.declare(total)?;
    sink.total_hint = total;

    for index in 0..archive.len() {
        sink.check_cancel()?;
        let mut entry = archive.by_index(index).map_err(map_error)?;
        let name = entry.name().map_or_else(
            |_| String::from_utf8_lossy(entry.name_raw()).into_owned(),
            |name| name.into_owned(),
        );
        if entry.is_symlink() {
            sink.skip_link(&name)?;
        } else if entry.is_dir() {
            sink.dir(&name)?;
        } else {
            let size = entry.size();
            sink.file(&name, &mut entry, size)?;
        }
    }
    Ok(())
}

fn map_error(error: ::zip::result::ZipError) -> ExtractError {
    use ::zip::result::ZipError;
    match error {
        ZipError::Io(source) => io("read", Path::new("archive"), source),
        ZipError::UnsupportedArchive(detail) => ExtractError::Unsupported {
            detail: format!("unsupported ZIP: {detail}"),
        },
        other => ExtractError::Corrupt {
            detail: other.to_string(),
        },
    }
}
