//! Manifest schema and persistence (`ARCHITECTURE.md` §9.4):
//!
//! ```text
//! <data_dir>/nuxd/
//!   queue.json                 package order, priorities, id counters, settings
//!   packages/<package_id>.json one manifest per package
//! ```
//!
//! This module only knows how to load and save one manifest file at an
//! explicit path — where `<data_dir>` actually is (via the `directories`
//! crate) is a driver concern, built in Phase 2.
//!
//! Saving is synchronous (`std::fs`), not `tokio::fs`: Phase 1 is
//! foundations without an async runtime (see the phase header in
//! `IMPLEMENTATION_PLAN.md`). The driver wraps these calls in
//! `spawn_blocking` from Phase 2 onward.

use crate::error::StorageError;
use crate::model::{Package, PackageId};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const QUEUE_SCHEMA_VERSION: u32 = 1;
pub const PACKAGE_SCHEMA_VERSION: u32 = 1;

/// `queue.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueueManifest {
    pub schema_version: u32,
    /// Package ids in queue order — the tie-break when two packages
    /// share a `Priority` (§4: "then queue order"). Each package's own
    /// priority lives on the `Package` in its own manifest file, not
    /// here.
    pub package_order: Vec<PackageId>,
    /// Monotonic counters the engine draws new ids from, so a restart
    /// never reuses one a completed or removed package's leftover state
    /// might still reference.
    pub next_package_id: u64,
    pub next_file_id: u64,
    pub settings: QueueSettings,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueueSettings {
    pub auto_resume_on_startup: bool,
    pub destination_dir: Option<PathBuf>,
}

impl Default for QueueSettings {
    fn default() -> Self {
        Self {
            auto_resume_on_startup: true,
            destination_dir: None,
        }
    }
}

impl QueueManifest {
    #[must_use]
    pub fn new() -> Self {
        Self {
            schema_version: QUEUE_SCHEMA_VERSION,
            package_order: Vec::new(),
            next_package_id: 0,
            next_file_id: 0,
            settings: QueueSettings::default(),
        }
    }
}

impl Default for QueueManifest {
    fn default() -> Self {
        Self::new()
    }
}

/// `packages/<package_id>.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PackageManifest {
    pub schema_version: u32,
    pub package: Package,
}

impl PackageManifest {
    #[must_use]
    pub fn new(package: Package) -> Self {
        Self {
            schema_version: PACKAGE_SCHEMA_VERSION,
            package,
        }
    }
}

/// Only the field every manifest version has had since v1 — enough to
/// decide how (or whether) to migrate the rest, without needing the
/// current Rust type to deserialize an old shape directly.
#[derive(Deserialize)]
struct VersionPeek {
    schema_version: u32,
}

/// Upgrades a manifest's raw JSON from `from_version` to `current_version`,
/// one step at a time. A no-op today — schema v1 is the only version
/// that has ever existed — but this is the hook a future bump attaches
/// its transform to, rather than every call site needing to know about
/// every past shape.
fn migrate(
    value: serde_json::Value,
    from_version: u32,
    current_version: u32,
    path: &str,
) -> Result<serde_json::Value, StorageError> {
    if from_version > current_version {
        return Err(StorageError::UnsupportedSchemaVersion {
            path: path.to_string(),
            found: from_version,
            supported: current_version,
        });
    }
    // No past versions to migrate from yet:
    // match from_version {
    //     1 => { /* transform the v1 shape into v2 here, then fall through */ }
    //     ...
    // }
    Ok(value)
}

fn tmp_path_for(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    PathBuf::from(name)
}

/// Fsyncs the directory entry itself so a rename is durable, not only
/// the file it points to. Renames aren't guaranteed to survive a crash
/// until the directory that holds them is synced too — Windows has no
/// direct equivalent, so this is a no-op there (matching §9.3's "sync
/// the directory (Unix)").
#[cfg(unix)]
fn sync_parent_dir(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent_dir(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Writes `bytes` to `path` durably: write the temp file, `sync_all` it,
/// rename over `path`, then sync the directory (§9.3's checkpoint
/// protocol, applied to one manifest file).
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), StorageError> {
    let path_str = path.display().to_string();
    let map_write_err = |source| StorageError::Write {
        path: path_str.clone(),
        source,
    };

    let tmp_path = tmp_path_for(path);
    let mut file = File::create(&tmp_path).map_err(map_write_err)?;
    file.write_all(bytes).map_err(map_write_err)?;
    file.sync_all().map_err(map_write_err)?;
    drop(file);

    fs::rename(&tmp_path, path).map_err(map_write_err)?;
    sync_parent_dir(path).map_err(map_write_err)?;
    Ok(())
}

/// Saves any manifest type as pretty JSON, atomically.
pub fn save_manifest<T: Serialize>(path: &Path, manifest: &T) -> Result<(), StorageError> {
    let path_str = path.display().to_string();
    let json = serde_json::to_vec_pretty(manifest).map_err(|source| StorageError::InvalidJson {
        path: path_str.clone(),
        source,
    })?;
    write_atomically(path, &json)
}

/// Loads and migrates a manifest. A truncated or corrupt file is
/// reported as an `Err`, never a panic — callers that load several
/// manifests (restoring every package at startup, say) are expected to
/// skip a failed one and keep going rather than let it take the rest
/// down; see `load_all_package_manifests` for that pattern applied.
pub fn load_manifest<T: DeserializeOwned>(
    path: &Path,
    current_version: u32,
) -> Result<T, StorageError> {
    let path_str = path.display().to_string();
    let bytes = fs::read(path).map_err(|source| StorageError::Read {
        path: path_str.clone(),
        source,
    })?;

    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|source| StorageError::InvalidJson {
            path: path_str.clone(),
            source,
        })?;
    let peek: VersionPeek =
        serde_json::from_value(value.clone()).map_err(|source| StorageError::InvalidJson {
            path: path_str.clone(),
            source,
        })?;

    let migrated = migrate(value, peek.schema_version, current_version, &path_str)?;
    serde_json::from_value(migrated).map_err(|source| StorageError::InvalidJson {
        path: path_str,
        source,
    })
}

pub fn save_queue_manifest(path: &Path, manifest: &QueueManifest) -> Result<(), StorageError> {
    save_manifest(path, manifest)
}

pub fn load_queue_manifest(path: &Path) -> Result<QueueManifest, StorageError> {
    load_manifest(path, QUEUE_SCHEMA_VERSION)
}

pub fn save_package_manifest(path: &Path, manifest: &PackageManifest) -> Result<(), StorageError> {
    save_manifest(path, manifest)
}

pub fn load_package_manifest(path: &Path) -> Result<PackageManifest, StorageError> {
    load_manifest(path, PACKAGE_SCHEMA_VERSION)
}

/// Loads every package manifest in `dir`, skipping (not failing on) any
/// file that doesn't parse — inspired by Plexo's `restorePersistedDownloads`,
/// which does the same so one corrupt download can't block every other
/// one from coming back after a restart. Also removes any leftover
/// `<id>.json.tmp` found beside a manifest: one surviving means the
/// process died mid-write, and the `.json` next to it — written by the
/// *previous* successful save — is what's trustworthy.
#[must_use]
pub fn load_all_package_manifests(dir: &Path) -> Vec<PackageManifest> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut manifests = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let _ = fs::remove_file(tmp_path_for(&path));
        if let Ok(manifest) = load_package_manifest(&path) {
            manifests.push(manifest);
        }
    }
    manifests
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FileEntry, FileId, PackageId};
    use tempfile::tempdir;

    fn sample_package(id: u64) -> Package {
        let file = FileEntry::new(FileId::new(1), "https://example.com/f", "f.bin").unwrap();
        Package::new(
            PackageId::new(id),
            "pkg",
            PathBuf::from("/tmp/dl"),
            vec![file],
        )
        .unwrap()
    }

    #[test]
    fn queue_manifest_round_trips() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("queue.json");
        let mut manifest = QueueManifest::new();
        manifest.package_order = vec![PackageId::new(1), PackageId::new(2)];
        manifest.next_package_id = 3;

        save_queue_manifest(&path, &manifest).unwrap();
        let loaded = load_queue_manifest(&path).unwrap();
        assert_eq!(loaded, manifest);
    }

    #[test]
    fn package_manifest_round_trips() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("1.json");
        let manifest = PackageManifest::new(sample_package(1));

        save_package_manifest(&path, &manifest).unwrap();
        let loaded = load_package_manifest(&path).unwrap();
        assert_eq!(loaded, manifest);
    }

    #[test]
    fn save_leaves_no_tmp_file_behind() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("1.json");
        save_package_manifest(&path, &PackageManifest::new(sample_package(1))).unwrap();
        assert!(!tmp_path_for(&path).exists());
        assert!(path.exists());
    }

    #[test]
    fn save_creates_valid_pretty_json() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("1.json");
        save_package_manifest(&path, &PackageManifest::new(sample_package(1))).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains('\n'), "expected pretty-printed JSON");
        let _: serde_json::Value = serde_json::from_str(&text).unwrap();
    }

    #[test]
    fn missing_file_is_a_read_error_not_a_panic() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("missing.json");
        let result = load_package_manifest(&path);
        assert!(matches!(result, Err(StorageError::Read { .. })));
    }

    #[test]
    fn truncated_json_is_reported_not_a_panic() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("1.json");
        fs::write(&path, b"{\"schema_version\": 1, \"package\": {").unwrap();
        let result = load_package_manifest(&path);
        assert!(matches!(result, Err(StorageError::InvalidJson { .. })));
    }

    #[test]
    fn corrupt_non_json_is_reported_not_a_panic() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("1.json");
        fs::write(&path, b"not json at all").unwrap();
        let result = load_package_manifest(&path);
        assert!(matches!(result, Err(StorageError::InvalidJson { .. })));
    }

    #[test]
    fn a_newer_schema_version_than_this_build_supports_is_reported() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("1.json");
        fs::write(&path, br#"{"schema_version": 999, "package": {}}"#).unwrap();
        let result = load_package_manifest(&path);
        assert!(matches!(
            result,
            Err(StorageError::UnsupportedSchemaVersion {
                found: 999,
                supported: 1,
                ..
            })
        ));
    }

    #[test]
    fn load_all_skips_corrupt_files_and_keeps_the_rest() {
        let dir = tempdir().unwrap();
        save_package_manifest(
            &dir.path().join("1.json"),
            &PackageManifest::new(sample_package(1)),
        )
        .unwrap();
        save_package_manifest(
            &dir.path().join("2.json"),
            &PackageManifest::new(sample_package(2)),
        )
        .unwrap();
        fs::write(dir.path().join("3.json"), b"not json").unwrap();
        // A non-.json file must be ignored outright, not attempted.
        fs::write(dir.path().join("readme.txt"), b"hello").unwrap();

        let mut loaded = load_all_package_manifests(dir.path());
        loaded.sort_by_key(|m| m.package.id);

        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].package.id, PackageId::new(1));
        assert_eq!(loaded[1].package.id, PackageId::new(2));
    }

    #[test]
    fn load_all_removes_a_leftover_tmp_file_beside_a_good_manifest() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("1.json");
        save_package_manifest(&path, &PackageManifest::new(sample_package(1))).unwrap();
        // Simulate a process that died mid-write on some *other* save,
        // leaving its temp file behind.
        fs::write(tmp_path_for(&path), b"half-written").unwrap();

        let loaded = load_all_package_manifests(dir.path());
        assert_eq!(loaded.len(), 1);
        assert!(!tmp_path_for(&path).exists());
    }

    #[test]
    fn load_all_on_a_missing_directory_returns_empty_not_a_panic() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert!(load_all_package_manifests(&missing).is_empty());
    }
}
