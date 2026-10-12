//! A download's own files on disk (`ARCHITECTURE.md` §9.1–9.2):
//! reserving a destination name, creating and preallocating the
//! in-progress file, positional writes into it, and finalizing it in
//! place once complete. No part files, no reassembly pass — each block
//! is written straight to its final offset.

use crate::error::StorageError;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

/// `mkdir -p`, tolerant of one Windows quirk: creating a path that's
/// already a directory (a drive root, say) can report an error there
/// even though the directory demonstrably exists — checked for and
/// treated as success rather than surfaced as a failure.
pub fn ensure_directory(dir: &Path) -> Result<(), StorageError> {
    match fs::create_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(source) if dir.is_dir() => {
            let _ = source;
            Ok(())
        }
        Err(source) => Err(StorageError::Io {
            operation: "create directory",
            path: dir.display().to_string(),
            source,
        }),
    }
}

/// Somewhere to stop rather than spin forever if every candidate name is
/// already taken (or something outside this app is creating them as
/// fast as it tries).
const MAX_NAME_ATTEMPTS: u32 = 10_000;

/// Claims `<dir>/<file_name>`, or `<dir>/<file_name> (1)`,
/// `<dir>/<file_name> (2)`, … if it's already taken, by creating an
/// empty file there with an exclusive create (`O_EXCL`-equivalent) —
/// merely *checking* the name is free would leave a race for two
/// downloads started around the same time to both pick it.
///
/// `file_name` is expected to already be sanitized (`probe::sanitize_filename`)
/// and be a single path component; this function doesn't sanitize it
/// itself, to keep filesystem and HTTP/naming concerns in separate
/// modules.
///
/// The caller owns the placeholder this creates from here: it must be
/// removed if the download doesn't end up producing a file (see
/// `finalize`, which replaces it on success).
pub fn reserve_destination_path(dir: &Path, file_name: &str) -> Result<PathBuf, StorageError> {
    ensure_directory(dir)?;

    let name_path = Path::new(file_name);
    let ext = name_path
        .extension()
        .map_or_else(String::new, |e| format!(".{}", e.to_string_lossy()));
    let stem = name_path.file_stem().map_or_else(
        || file_name.to_string(),
        |s| s.to_string_lossy().into_owned(),
    );

    for counter in 0..MAX_NAME_ATTEMPTS {
        let candidate = if counter == 0 {
            dir.join(file_name)
        } else {
            dir.join(format!("{stem} ({counter}){ext}"))
        };

        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(_) => return Ok(candidate),
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(StorageError::Io {
                    operation: "reserve destination name",
                    path: candidate.display().to_string(),
                    source,
                });
            }
        }
    }

    Err(StorageError::NameReservationExhausted {
        dir: dir.display().to_string(),
        name: file_name.to_string(),
    })
}

/// Creates `path` and, when `size` is known and non-zero, preallocates
/// it to that length. `set_len` alone produces a sparse file on ext4,
/// btrfs, xfs and APFS; on NTFS it doesn't unless the file is first
/// marked sparse (`mark_sparse`, best-effort — a failure there just
/// means slower, eagerly-zero-filled preallocation, not a broken
/// download). An unknown size isn't preallocated; it's written
/// sequentially instead.
pub fn create_and_preallocate(path: &Path, size: Option<u64>) -> Result<File, StorageError> {
    // Read and write: byte sampling (§10) reads durable regions back.
    // `create_new` (O_EXCL) never follows a symlink at `path`. A regular
    // leftover from an earlier run is replaced; anything else is refused.
    let open = || {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
    };
    let io_error = |source| StorageError::Io {
        operation: "create",
        path: path.display().to_string(),
        source,
    };
    let file = match open() {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let meta = fs::symlink_metadata(path).map_err(io_error)?;
            if !meta.file_type().is_file() {
                return Err(io_error(std::io::Error::other(
                    "refusing to replace a link or directory",
                )));
            }
            fs::remove_file(path).map_err(io_error)?;
            open().map_err(io_error)?
        }
        Err(error) => return Err(io_error(error)),
    };

    if let Some(size) = size
        && size > 0
    {
        mark_sparse(&file);
        file.set_len(size).map_err(|source| StorageError::Io {
            operation: "preallocate",
            path: path.display().to_string(),
            source,
        })?;
    }

    Ok(file)
}

#[cfg(windows)]
fn mark_sparse(file: &File) {
    let _ = windows_sparse::set_sparse(file);
}

#[cfg(not(windows))]
fn mark_sparse(_file: &File) {
    // ext4, btrfs, xfs and APFS all create a sparse file from `set_len`
    // alone — nothing to do here off Windows.
}

#[cfg(windows)]
#[allow(unsafe_code)] // the one FFI call ARCHITECTURE.md §9.1 carves out the `unsafe_code` deny for
mod windows_sparse {
    use std::fs::File;
    use std::io;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::FSCTL_SET_SPARSE;
    use windows_sys::Win32::System::IO::DeviceIoControl;

    /// Marks `file` sparse via `FSCTL_SET_SPARSE`, so a later `set_len`
    /// extends it without the OS eagerly zero-filling every byte up to
    /// the new length first.
    ///
    /// Not independently verified on Windows — written and reviewed on
    /// Linux with no Windows toolchain available to compile or run it;
    /// confirm it on Windows CI (`IMPLEMENTATION_PLAN.md` P0.4) or a real
    /// machine before relying on it.
    pub fn set_sparse(file: &File) -> io::Result<()> {
        let handle = file.as_raw_handle() as HANDLE;
        let mut bytes_returned: u32 = 0;
        // SAFETY: `handle` is a valid, open file HANDLE for as long as
        // `file` is alive, which outlives this call. FSCTL_SET_SPARSE
        // takes no input or output buffer, so null pointers with
        // zero-length are exactly what its contract calls for.
        let ok = unsafe {
            DeviceIoControl(
                handle,
                FSCTL_SET_SPARSE,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                0,
                &raw mut bytes_returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

/// A positional write: `data` lands at `offset` in `file`, regardless of
/// where any other write to it landed. Multiple writes to disjoint
/// ranges of the same file, in any order or concurrently from separate
/// handles, produce the same result as writing them in order — the
/// property in-place block writes and hedging (§9.1) depend on.
#[cfg(unix)]
pub fn write_at(file: &File, path: &Path, offset: u64, data: &[u8]) -> Result<(), StorageError> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(data, offset)
        .map_err(|source| StorageError::Io {
            operation: "write",
            path: path.display().to_string(),
            source,
        })
}

/// As the Unix version, but looping: `seek_write` is not guaranteed to
/// write everything in one call, unlike `write_all_at`.
#[cfg(windows)]
pub fn write_at(file: &File, path: &Path, offset: u64, data: &[u8]) -> Result<(), StorageError> {
    use std::os::windows::fs::FileExt;
    let map_err = |source: io::Error| StorageError::Io {
        operation: "write",
        path: path.display().to_string(),
        source,
    };

    let mut written = 0usize;
    while written < data.len() {
        let n = file
            .seek_write(&data[written..], offset + written as u64)
            .map_err(map_err)?;
        if n == 0 {
            return Err(map_err(io::Error::new(
                io::ErrorKind::WriteZero,
                "seek_write wrote 0 bytes",
            )));
        }
        written += n;
    }
    Ok(())
}

/// Reads exactly `buf.len()` bytes at `offset` — the counterpart of
/// `write_at`, used to compare durable bytes with a fresh sample (§10).
#[cfg(unix)]
pub fn read_at(file: &File, path: &Path, offset: u64, buf: &mut [u8]) -> Result<(), StorageError> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)
        .map_err(|source| StorageError::Io {
            operation: "read",
            path: path.display().to_string(),
            source,
        })
}

/// As the Unix version, but looping: `seek_read` may return short.
#[cfg(windows)]
pub fn read_at(file: &File, path: &Path, offset: u64, buf: &mut [u8]) -> Result<(), StorageError> {
    use std::os::windows::fs::FileExt;
    let map_err = |source: io::Error| StorageError::Io {
        operation: "read",
        path: path.display().to_string(),
        source,
    };
    let mut filled = 0usize;
    while filled < buf.len() {
        let n = file
            .seek_read(&mut buf[filled..], offset + filled as u64)
            .map_err(map_err)?;
        if n == 0 {
            return Err(map_err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "seek_read hit end of file",
            )));
        }
        filled += n;
    }
    Ok(())
}

/// Flushes `file`'s data to durable storage (not necessarily its
/// metadata — see `File::sync_data`), the step the checkpoint protocol
/// (§9.3) requires before a manifest can claim the bytes it just wrote.
pub fn sync_data(file: &File, path: &Path) -> Result<(), StorageError> {
    file.sync_data().map_err(|source| StorageError::Io {
        operation: "sync",
        path: path.display().to_string(),
        source,
    })
}

/// Renames `part_path` (the in-progress `.nuxdpart` file) over
/// `final_path` (the placeholder `reserve_destination_path` created),
/// completing the download. `std::fs::rename` replaces an existing
/// destination file on every platform this project targets, so no
/// separate remove-then-rename step is needed.
pub fn finalize(part_path: &Path, final_path: &Path) -> Result<(), StorageError> {
    fs::rename(part_path, final_path).map_err(|source| StorageError::Io {
        operation: "finalize (rename)",
        path: final_path.display().to_string(),
        source,
    })
}

/// Bytes available (to an unprivileged process) on the filesystem that
/// holds `dir`, for the upfront disk-space check (§9.2).
pub fn available_space(dir: &Path) -> Result<u64, StorageError> {
    fs4::available_space(dir).map_err(|source| StorageError::Io {
        operation: "check free space",
        path: dir.display().to_string(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tempfile::tempdir;

    #[test]
    fn ensure_directory_creates_nested_dirs() {
        let dir = tempdir().unwrap();
        let nested = dir.path().join("a").join("b").join("c");
        ensure_directory(&nested).unwrap();
        assert!(nested.is_dir());
    }

    #[test]
    fn ensure_directory_on_an_existing_dir_is_fine() {
        let dir = tempdir().unwrap();
        ensure_directory(dir.path()).unwrap();
        ensure_directory(dir.path()).unwrap();
    }

    #[test]
    fn reserve_claims_the_plain_name_first() {
        let dir = tempdir().unwrap();
        let path = reserve_destination_path(dir.path(), "report.pdf").unwrap();
        assert_eq!(path, dir.path().join("report.pdf"));
        assert!(path.exists());
    }

    #[test]
    fn reserve_falls_back_to_a_numbered_name_on_collision() {
        let dir = tempdir().unwrap();
        let first = reserve_destination_path(dir.path(), "report.pdf").unwrap();
        let second = reserve_destination_path(dir.path(), "report.pdf").unwrap();
        let third = reserve_destination_path(dir.path(), "report.pdf").unwrap();

        assert_eq!(first, dir.path().join("report.pdf"));
        assert_eq!(second, dir.path().join("report (1).pdf"));
        assert_eq!(third, dir.path().join("report (2).pdf"));
        assert!(first.exists() && second.exists() && third.exists());
    }

    #[test]
    fn reserve_handles_a_name_with_no_extension() {
        let dir = tempdir().unwrap();
        let first = reserve_destination_path(dir.path(), "README").unwrap();
        let second = reserve_destination_path(dir.path(), "README").unwrap();
        assert_eq!(first, dir.path().join("README"));
        assert_eq!(second, dir.path().join("README (1)"));
    }

    #[test]
    fn create_and_preallocate_sets_the_exact_length() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("file.bin");
        let file = create_and_preallocate(&path, Some(1024)).unwrap();
        drop(file);
        assert_eq!(fs::metadata(&path).unwrap().len(), 1024);
    }

    #[test]
    fn create_and_preallocate_with_unknown_size_creates_an_empty_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("file.bin");
        let file = create_and_preallocate(&path, None).unwrap();
        drop(file);
        assert_eq!(fs::metadata(&path).unwrap().len(), 0);
    }

    #[test]
    fn writes_land_at_their_offset_regardless_of_order() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("file.bin");
        let file = create_and_preallocate(&path, Some(11)).unwrap();

        // Deliberately out of order: the property under test is that
        // position in the call sequence doesn't matter, only `offset`.
        write_at(&file, &path, 6, b"world").unwrap();
        write_at(&file, &path, 0, b"hello ").unwrap();

        drop(file);
        let mut contents = Vec::new();
        File::open(&path)
            .unwrap()
            .read_to_end(&mut contents)
            .unwrap();
        assert_eq!(&contents, b"hello world");
    }

    #[test]
    fn write_and_sync_then_read_back_the_exact_bytes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("file.bin");
        let file = create_and_preallocate(&path, Some(5)).unwrap();
        write_at(&file, &path, 0, b"abcde").unwrap();
        sync_data(&file, &path).unwrap();
        drop(file);

        let mut contents = Vec::new();
        File::open(&path)
            .unwrap()
            .read_to_end(&mut contents)
            .unwrap();
        assert_eq!(contents, b"abcde");
    }

    #[test]
    fn finalize_replaces_the_placeholder_with_the_downloaded_bytes() {
        let dir = tempdir().unwrap();
        let final_path = reserve_destination_path(dir.path(), "movie.mp4").unwrap();
        let part_path = dir.path().join("movie.mp4.nuxdpart");

        let file = create_and_preallocate(&part_path, Some(4)).unwrap();
        write_at(&file, &part_path, 0, b"data").unwrap();
        drop(file);

        finalize(&part_path, &final_path).unwrap();

        assert!(!part_path.exists());
        let mut contents = Vec::new();
        File::open(&final_path)
            .unwrap()
            .read_to_end(&mut contents)
            .unwrap();
        assert_eq!(contents, b"data");
    }

    #[test]
    fn available_space_returns_a_plausible_number() {
        let dir = tempdir().unwrap();
        let space = available_space(dir.path()).unwrap();
        assert!(space > 0, "expected some free space on the temp filesystem");
    }

    #[test]
    fn name_reservation_gives_up_after_the_attempt_limit() {
        let dir = tempdir().unwrap();
        // Pre-create every candidate name reserve_destination_path would
        // try, so it's forced to exhaust its budget.
        File::create(dir.path().join("f.bin")).unwrap();
        for n in 1..MAX_NAME_ATTEMPTS {
            File::create(dir.path().join(format!("f ({n}).bin"))).unwrap();
        }
        let result = reserve_destination_path(dir.path(), "f.bin");
        assert!(matches!(
            result,
            Err(StorageError::NameReservationExhausted { .. })
        ));
    }
}
