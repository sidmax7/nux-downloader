//! The blocking disk work behind `PrepareFile`, `Checkpoint` and
//! `Finalize` (`ARCHITECTURE.md` §7.3, §9.1–§9.3). Every function here
//! runs on tokio's blocking pool, never on the driver task itself.

use crate::engine::FilePrep;
use crate::error::StorageError;
use crate::model::PackageId;
use crate::storage::file::{
    available_space, create_and_preallocate, ensure_directory, finalize, reserve_destination_path,
    sync_data,
};
use crate::storage::manifest::{PackageManifest, save_package_manifest};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Suffix of the in-progress file written next to its reserved final
/// name (§9.1).
pub const PART_SUFFIX: &str = ".nuxdpart";

/// A prepared file the driver keeps open for positional writes until it
/// is finalized.
#[derive(Debug, Clone)]
pub struct OpenFile {
    pub file: Arc<File>,
    pub part_path: PathBuf,
    pub final_path: PathBuf,
    pub package: PackageId,
}

#[must_use]
pub fn part_path_for(final_path: &Path) -> PathBuf {
    let mut name = final_path.as_os_str().to_owned();
    name.push(PART_SUFFIX);
    PathBuf::from(name)
}

/// Checks free space, reserves `<dest>/<name>` with an exclusive create,
/// then creates and preallocates `<name>.nuxdpart` beside it (§9.2). The
/// placeholder is removed again if the part file can't be created, so a
/// failed activation leaves nothing behind.
pub fn prepare(plan: &FilePrep) -> Result<OpenFile, StorageError> {
    check_space(&plan.dest_dir, plan.expected_size)?;
    let final_path = reserve_destination_path(&plan.dest_dir, &plan.suggested_name)?;
    let part_path = part_path_for(&final_path);
    create_part(&final_path, &part_path, plan.expected_size, plan.package)
}

/// Reopens an already-reserved `.nuxdpart` for a file resuming after a
/// restart (§9.4), instead of reserving a fresh name and creating one —
/// `final_name` is the exact name a previous run already settled on.
/// Falls back to creating fresh if the part file is missing outright
/// (nothing to reopen). The returned `bool` is whether the reopened
/// file's length matches what the layout expects; `false` means its
/// durable frontiers can't be trusted and the caller must reset them to
/// zero before resuming into it.
pub fn reopen(plan: &FilePrep, final_name: &str) -> Result<(OpenFile, bool), StorageError> {
    let final_path = plan.dest_dir.join(final_name);
    let part_path = part_path_for(&final_path);

    // Never write through a symlink standing where our partial file
    // should be.
    if std::fs::symlink_metadata(&part_path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(StorageError::Io {
            operation: "open",
            path: part_path.display().to_string(),
            source: std::io::Error::other("refusing to follow a symlink"),
        });
    }
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&part_path)
    {
        Ok(file) => file,
        Err(_) => {
            check_space(&plan.dest_dir, plan.expected_size)?;
            let open = create_part(&final_path, &part_path, plan.expected_size, plan.package)?;
            return Ok((open, false));
        }
    };

    let matches = match plan.expected_size {
        Some(size) => file
            .metadata()
            .map(|meta| meta.len() == size)
            .unwrap_or(false),
        None => true,
    };
    Ok((
        OpenFile {
            file: Arc::new(file),
            part_path,
            final_path,
            package: plan.package,
        },
        matches,
    ))
}

fn check_space(dest_dir: &Path, expected_size: Option<u64>) -> Result<(), StorageError> {
    ensure_directory(dest_dir)?;
    let Some(needed) = expected_size else {
        return Ok(());
    };
    let available = available_space(dest_dir)?;
    if available < needed {
        return Err(StorageError::InsufficientSpace {
            dir: dest_dir.display().to_string(),
            needed,
            available,
        });
    }
    Ok(())
}

fn create_part(
    final_path: &Path,
    part_path: &Path,
    expected_size: Option<u64>,
    package: PackageId,
) -> Result<OpenFile, StorageError> {
    match create_and_preallocate(part_path, expected_size) {
        Ok(file) => Ok(OpenFile {
            file: Arc::new(file),
            part_path: part_path.to_path_buf(),
            final_path: final_path.to_path_buf(),
            package,
        }),
        Err(error) => {
            let _ = std::fs::remove_file(part_path);
            let _ = std::fs::remove_file(final_path);
            Err(error)
        }
    }
}

/// The driver's half of §9.3's checkpoint: `sync_data` every open file of
/// the package, *then* write the manifest — whose snapshot was taken by
/// the engine before this ran, so it never claims a byte that isn't
/// durable once the syncs return.
pub fn checkpoint(
    files: &[(Arc<File>, PathBuf)],
    manifest_dir: &Path,
    manifest: &PackageManifest,
) -> Result<(), StorageError> {
    for (file, path) in files {
        sync_data(file, path)?;
    }
    ensure_directory(manifest_dir)?;
    save_package_manifest(&manifest_path(manifest_dir, manifest.package.id), manifest)
}

#[must_use]
pub fn manifest_path(manifest_dir: &Path, package: PackageId) -> PathBuf {
    manifest_dir.join(format!("{package}.json"))
}

/// Syncs the finished file, then renames `.nuxdpart` over the reserved
/// placeholder (§9.2).
pub fn finish(open: &OpenFile) -> Result<(), StorageError> {
    sync_data(&open.file, &open.part_path)?;
    finalize(&open.part_path, &open.final_path)
}
