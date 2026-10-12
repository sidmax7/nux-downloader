//! What the engine asks the driver to do (`ARCHITECTURE.md` §14.1). Every
//! effect is a request, never a promise — the driver executes it and
//! reports back through the matching `Input` variant (`ProbeDone`,
//! `FilePrepared`, `CheckpointDone`, `AttemptEnded`), except `Finalize`
//! and `Emit`, which are fire-and-forget.

use crate::model::{ByteRange, FileError, FileId, LinkId, PackageError, PackageId};
use crate::storage::manifest::PackageManifest;
use std::path::PathBuf;
use std::time::Instant;
use url::Url;

pub use super::input::AbortReason;

/// What `PrepareFile` asks the driver to do (§7.3, §9.1, §9.2): reserve
/// the destination name, create the `.nuxdpart` placeholder, and
/// preallocate it. Layout planning itself already happened in the pure
/// core, right after the probe that produced `expected_size`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePrep {
    /// The package the file belongs to — the driver orders each
    /// package's disk I/O (checkpoints, finalizes) by it.
    pub package: PackageId,
    pub dest_dir: PathBuf,
    pub suggested_name: String,
    pub expected_size: Option<u64>,
    /// `Some(name)` when this is a file resuming after a restart (§9.4):
    /// reopen `<dest_dir>/<name>.nuxdpart`, which a previous run already
    /// reserved and (partly) wrote, instead of reserving a fresh name and
    /// creating one. `None` is the normal first-activation path.
    pub resume_final_name: Option<String>,
}

/// What `PrepareFile` reports back: the name it actually reserved, which
/// may differ from `suggested_name` if that name was already taken
/// (§9.2's `name (1).ext` fallback).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedFile {
    pub final_name: String,
    /// `false` only for a resumed file whose `.nuxdpart` was missing or
    /// the wrong length (§9.4: "otherwise reset its frontiers to zero") —
    /// the durable progress a restored manifest claimed can't be trusted,
    /// so every block's frontier must go back to zero before anything
    /// resumes from it. Always `true` for a freshly created file, whose
    /// parts start at zero anyway.
    pub frontiers_valid: bool,
}

/// Observable state changes, for a UI or CLI to subscribe to (§15).
/// Carries ids, not full snapshots — a subscriber that wants the current
/// state reads it from the engine's own query API rather than
/// reconstructing it from a stream of events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineEvent {
    /// The set of usable links changed (P3.4); read the new set from a
    /// snapshot.
    LinksChanged,
    PackageAdded {
        package: PackageId,
    },
    FileProbed {
        file: FileId,
    },
    FileActive {
        file: FileId,
    },
    FileVerified {
        file: FileId,
    },
    FileFailed {
        file: FileId,
        error: FileError,
    },
    PackagePaused {
        package: PackageId,
    },
    PackageResumed {
        package: PackageId,
    },
    PackageDone {
        package: PackageId,
    },
    /// Every file arrived; the package is being verified (§12.3).
    PackageVerifying {
        package: PackageId,
    },
    /// Extraction started (§12.4).
    PackageExtracting {
        package: PackageId,
    },
    /// Extraction progress, announced by the driver as bytes are written.
    ExtractProgress {
        package: PackageId,
        bytes_done: u64,
        bytes_total: u64,
    },
    PackageRemoved {
        package: PackageId,
    },
    /// A failed file was reset and will be probed again.
    FileRestarted {
        file: FileId,
    },
    /// A package's priority changed (`Command::SetPriority`).
    PackagePriorityChanged {
        package: PackageId,
    },
    /// The queue order changed (`Command::MoveTo`); read the new order
    /// from a snapshot.
    QueueReordered,
    PackageFailed {
        package: PackageId,
        error: PackageError,
    },
}

/// What the driver should do next. Pure `EngineState::handle` never
/// performs any of these itself — see `ARCHITECTURE.md` §14.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    Probe {
        file: FileId,
        url: Url,
        /// `None` routes through the shared default client (§6.3); once
        /// links are real (Phase 3), a probe can be pinned to one.
        via: Option<LinkId>,
    },
    PrepareFile {
        file: FileId,
        plan: FilePrep,
    },
    /// Deletes a restarted file's partial data and placeholder (the file
    /// is being started over). Fire-and-forget.
    DiscardFile {
        file: FileId,
        dest_dir: PathBuf,
        name: String,
    },
    /// Resolve and probe `source` again (§13): a `403`/`410` on a signed
    /// URL means it expired. Reported back as `Input::Reresolved`.
    Reresolve {
        file: FileId,
        source: String,
    },
    StartAttempt {
        id: crate::model::AttemptId,
        file: FileId,
        url: Url,
        range: ByteRange,
        file_offset: u64,
        link: LinkId,
    },
    AbortAttempt {
        id: crate::model::AttemptId,
        reason: AbortReason,
    },
    Checkpoint {
        package: PackageId,
        manifest: PackageManifest,
    },
    /// Renames `<name>.nuxdpart` over the reserved placeholder (§9.2).
    /// Fire-and-forget: a rename failure has no `Input` to report back
    /// through in this phase (matches `ARCHITECTURE.md` §14.1's `Input`
    /// enum, which has no `FinalizeDone`); the file already sits at
    /// `Verified` once every block is durable, since a checkpoint has
    /// already made those bytes crash-safe.
    Finalize {
        file: FileId,
    },
    /// Verify a package whose files are all on disk (§12.3); reported
    /// back as `Input::VerifyDone`. The driver first waits for every
    /// earlier rename to land.
    VerifyPackage {
        package: PackageId,
        dest_dir: PathBuf,
        files: Vec<crate::verify::VerifyFile>,
    },
    /// Extract a verified package's archive (§12.4); progress is
    /// announced as events and the end is reported as `Input::ExtractDone`.
    Extract {
        package: PackageId,
        spec: crate::extract::ExtractSpec,
    },
    /// Stop verification or extraction of a package that was cancelled
    /// or removed.
    CancelPost {
        package: PackageId,
    },
    /// Deletes files from a directory: archives after a successful
    /// extraction, or a file that failed verification (so a restart
    /// fetches it afresh). Fire-and-forget.
    DeleteFiles {
        dest_dir: PathBuf,
        names: Vec<String>,
    },
    Emit(EngineEvent),
    /// A hint that a timer should fire no later than this instant (retry
    /// backoff, §7.6). Correctness doesn't depend on the driver acting on
    /// it — `EngineState` re-checks every block's backoff against `now`
    /// on its own on every `handle` call, so a driver that just ticks on
    /// its normal cadence (§19: 250 ms) still retries on time; this is
    /// only useful for a driver that wants to wake early instead.
    WakeAt(Instant),
}
