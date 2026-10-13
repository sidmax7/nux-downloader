//! Archive extraction (`ARCHITECTURE.md` §12.4). Blocking: the driver
//! runs it on the blocking pool, one package at a time.
//!
//! Entries are written into a private staging directory next to the
//! destination and only moved into place once the whole archive
//! extracted, so a failure, a cancel or a crash never leaves a
//! half-extracted tree among the user's files (a restart simply extracts
//! again). Nothing is ever overwritten: a name already taken gets a
//! ` (1)` suffix.
//!
//! Supported: ZIP, 7z (incl. `.7z.001…` volumes), generic `.001` splits
//! (joined; a joined ZIP or 7z is extracted), and RAR behind the `rar`
//! feature. Not supported: encrypted archives, and Info-ZIP split
//! archives (`x.z01`), whose offsets are per-disk.

mod sevenz;
pub mod sink;
mod volumes;
mod zip;

#[cfg(feature = "rar")]
mod rar;

use crate::error::ExtractError;
use crate::model::ArchiveScheme;
use sink::{Limits, Progress, Sink, io};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

pub use sink::{Limits as ExtractLimits, Progress as ExtractProgress};
pub use volumes::MultiVolumeReader;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractSpec {
    pub scheme: ArchiveScheme,
    pub base_name: String,
    /// Volume files in reading order (see `group`).
    pub volumes: Vec<PathBuf>,
    pub dest_dir: PathBuf,
    /// An archive whose root is a single folder has that folder's
    /// contents placed in `dest_dir`, so a package folder doesn't end up
    /// holding `Name/Name/…` (Decision 37). Only for a package's own
    /// folder: in a shared download folder the archive's folder is what
    /// keeps its files together.
    pub unwrap_single_dir: bool,
    pub limits: Limits,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExtractSummary {
    pub entries: u64,
    pub bytes: u64,
    /// Symlinks (and similar) that were deliberately not created.
    pub skipped_links: u64,
}

const ZIP_MAGIC: &[u8] = b"PK\x03\x04";
const SEVENZ_MAGIC: &[u8] = b"7z\xBC\xAF\x27\x1C";

/// Extracts the archive described by `spec` into `spec.dest_dir`.
pub fn extract(
    spec: &ExtractSpec,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
) -> Result<ExtractSummary, ExtractError> {
    if spec.volumes.is_empty() {
        return Err(ExtractError::Unsupported {
            detail: "the archive has no volumes".to_string(),
        });
    }
    let staging = staging_dir(&spec.dest_dir, &spec.base_name);
    remove_stale(&staging);

    let outcome = extract_into(spec, &staging, cancel, progress);
    let result = match outcome {
        Ok(summary) => publish(
            &publish_root(&staging, spec.unwrap_single_dir),
            &spec.dest_dir,
        )
        .map(|()| summary),
        Err(error) => Err(error),
    };
    remove_stale(&staging);
    result
}

/// Clears a leftover staging path from an earlier run. A symlink planted
/// there is removed as a link — never followed — so extraction can't be
/// steered into some other directory.
fn remove_stale(path: &Path) {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_dir() => {
            let _ = std::fs::remove_dir_all(path);
        }
        Ok(_) => {
            let _ = std::fs::remove_file(path);
        }
        Err(_) => {}
    }
}

fn staging_dir(dest_dir: &Path, base_name: &str) -> PathBuf {
    let safe: String = base_name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    dest_dir.join(format!(".{safe}.nuxdextract"))
}

fn extract_into(
    spec: &ExtractSpec,
    staging: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
) -> Result<ExtractSummary, ExtractError> {
    let archive_size: u64 = spec
        .volumes
        .iter()
        .filter_map(|v| std::fs::metadata(v).ok())
        .map(|m| m.len())
        .sum();
    let mut sink = Sink::new(staging, spec.limits, archive_size, cancel, progress)?;

    let reader = || {
        MultiVolumeReader::new(spec.volumes.clone()).map_err(|e| io("open", &spec.volumes[0], e))
    };
    match spec.scheme {
        ArchiveScheme::Rar | ArchiveScheme::RarLegacy => {
            #[cfg(feature = "rar")]
            rar::extract(&spec.volumes[0], &mut sink)?;
            #[cfg(not(feature = "rar"))]
            return Err(ExtractError::Unsupported {
                detail: "this build was made without RAR support; extract the archive \
                         with another tool, or install the release build"
                    .to_string(),
            });
        }
        ArchiveScheme::SevenZip => sevenz::extract(reader()?, &mut sink)?,
        ArchiveScheme::SplitZip => {
            if spec.volumes.len() > 1 {
                return Err(ExtractError::Unsupported {
                    detail: "split ZIP volumes (.z01, .z02, …) aren't supported; \
                             re-join them with `zip -s 0 in.zip --out joined.zip`"
                        .to_string(),
                });
            }
            zip::extract(reader()?, &mut sink)?;
        }
        ArchiveScheme::GenericSplit => {
            let mut joined = reader()?;
            let mut magic = [0u8; 6];
            let got = std::io::Read::read(&mut joined, &mut magic).unwrap_or(0);
            std::io::Seek::seek(&mut joined, std::io::SeekFrom::Start(0))
                .map_err(|e| io("read", &spec.volumes[0], e))?;
            if magic[..got].starts_with(ZIP_MAGIC) {
                zip::extract(joined, &mut sink)?;
            } else if magic[..got].starts_with(SEVENZ_MAGIC) {
                sevenz::extract(joined, &mut sink)?;
            } else {
                // Not an archive we know: the job is just to join.
                let length = joined.len();
                sink.total_hint = length;
                sink.file(&spec.base_name, &mut joined, length)?;
            }
        }
    }
    Ok(ExtractSummary {
        entries: sink.entries,
        bytes: sink.written,
        skipped_links: sink.skipped_links,
    })
}

/// Where the entries to publish are: the staging directory, or the one
/// folder it holds when unwrapping is asked for and that is all it holds.
fn publish_root(staging: &Path, unwrap_single_dir: bool) -> PathBuf {
    if unwrap_single_dir && let Ok(entries) = std::fs::read_dir(staging) {
        let entries: Vec<_> = entries.filter_map(Result::ok).take(2).collect();
        if let [only] = entries.as_slice()
            && only.file_type().is_ok_and(|t| t.is_dir())
        {
            return only.path();
        }
    }
    staging.to_path_buf()
}

/// Moves the staged top-level entries into `dest_dir`, never replacing
/// anything that is already there.
fn publish(staging: &Path, dest_dir: &Path) -> Result<(), ExtractError> {
    let entries = std::fs::read_dir(staging).map_err(|e| io("read", staging, e))?;
    for entry in entries {
        let entry = entry.map_err(|e| io("read", staging, e))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let target = free_name(dest_dir, &name);
        std::fs::rename(entry.path(), &target).map_err(|e| io("move", &target, e))?;
    }
    Ok(())
}

/// `name`, or `stem (n).ext` for the first `n` that is free.
fn free_name(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if std::fs::symlink_metadata(&first).is_err() {
        return first;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(dot) if dot > 0 => (&name[..dot], &name[dot..]),
        _ => (name, ""),
    };
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| std::fs::symlink_metadata(p).is_err())
        .unwrap_or(first)
}
