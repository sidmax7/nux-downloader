//! One `nuxd` per data directory. Two engines writing the same queue and
//! package manifests would overwrite each other's checkpoints, so every
//! engine — a running download or a management session — holds an
//! exclusive advisory lock on `<data dir>/nuxd.lock` for as long as it
//! lives. The OS drops the lock if the process dies, so a crash never
//! leaves a stale one; the file also records the holder's process id for
//! the error message.

use crate::error::StorageError;
use fs4::fs_std::FileExt;
use std::io::{Read, Write};
use std::path::Path;

/// Held for the engine's lifetime; dropping it releases the lock.
#[derive(Debug)]
pub struct DataDirLock {
    _file: std::fs::File,
}

impl DataDirLock {
    pub fn acquire(data_dir: &Path) -> Result<Self, StorageError> {
        let path = data_dir.join("nuxd.lock");
        let io = |operation, source| StorageError::Io {
            operation,
            path: path.display().to_string(),
            source,
        };
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| io("open the lock file", e))?;
        match file.try_lock_exclusive() {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == fs4::lock_contended_error().raw_os_error() => {
                let mut holder = String::new();
                let _ = file.read_to_string(&mut holder);
                return Err(StorageError::Locked {
                    dir: data_dir.display().to_string(),
                    holder: holder.trim().parse().ok(),
                });
            }
            Err(e) => return Err(io("lock", e)),
        }
        // Ours now: record who we are.
        let _ = file.set_len(0);
        let _ = std::io::Seek::rewind(&mut file);
        let _ = write!(file, "{}", std::process::id());
        let _ = file.flush();
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_holder_is_refused_until_the_first_lets_go() {
        let dir = tempfile::tempdir().unwrap();
        let first = DataDirLock::acquire(dir.path()).unwrap();
        match DataDirLock::acquire(dir.path()) {
            Err(StorageError::Locked { holder, .. }) => {
                assert_eq!(holder, Some(std::process::id()));
            }
            other => panic!("expected Locked, got {other:?}"),
        }
        drop(first);
        DataDirLock::acquire(dir.path()).unwrap();
    }

    #[test]
    fn different_directories_do_not_interfere() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let _a = DataDirLock::acquire(a.path()).unwrap();
        let _b = DataDirLock::acquire(b.path()).unwrap();
    }
}
