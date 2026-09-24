//! What can happen to the engine (`ARCHITECTURE.md` §14.1): commands from
//! a shell, and the results of effects the driver executed. `Input` and
//! `Command` are `EngineState::handle`'s only way in.

use crate::error::{ProbeError, StorageError, TransferError};
use crate::model::{AttemptId, FileId, Link, PackageId, Priority};
use crate::probe::ProbeResult;
use std::path::PathBuf;

/// A user- or shell-issued command (§15's `Engine` API surface, minus the
/// read-only queries — those don't go through `handle` at all).
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Adds a package with one file per source URL. Files are probed
    /// immediately (§5: "Files can also be added straight to the queue,
    /// in which case they are probed on activation" — M1 has nothing else
    /// competing for the probe pool, so "activation" here is immediate;
    /// the probe *pool*'s bounded concurrency is P4.4's job). A source
    /// that isn't a valid URL still becomes a file, already `Failed`, so
    /// package indices and file counts stay predictable.
    AddPackage {
        name: String,
        dest_dir: PathBuf,
        sources: Vec<String>,
        priority: Priority,
    },
    Pause(PackageId),
    Resume(PackageId),
    Cancel(PackageId),
    /// Drops a package from engine state entirely, aborting any attempts
    /// first. Deleting its files and manifest from disk isn't the pure
    /// core's job — the driver does that itself once it sees the package
    /// is gone (§15's `Engine::remove`, `delete_files`).
    Remove(PackageId),
    /// Checkpoints every package one last time; the driver is
    /// responsible for actually exiting once the returned effects are
    /// executed.
    Shutdown,
}

/// How one transfer attempt ended (§8 step 6). `position` on the
/// `AttemptEnded` input this rides in is the attempt's own field, not
/// part of this type, so both a success and a failure always report the
/// same durable-frontier information regardless of outcome.
#[derive(Debug)]
pub enum AttemptOutcome {
    /// The requested range was fully transferred and validated.
    Success,
    Failed(TransferError),
}

/// Why the driver should abort an in-flight attempt (§14.1's `Effect`
/// doc comment). `Refresh` and `Lost` are unused until hedging lands
/// (P7.1) but are part of the type now so `Effect::AbortAttempt` doesn't need to change shape then.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbortReason {
    Pause,
    Cancel,
    /// Lost a hedge race to a faster attempt on another link (P7.1).
    Lost,
    /// Aborted by the slow-connection refresh (P7.1/P7.2).
    Refresh,
    /// The link it was running on disappeared (P3.4).
    LinkLost,
    /// The package hit a fatal (disk/IO) error and every attempt on it
    /// is being torn down (§16: "Disk full / I/O error — Fatal (package)").
    Fatal,
}

/// Everything that can happen to `EngineState`.
#[derive(Debug)]
pub enum Input {
    Command(Command),
    /// Sampled attempt positions, taken by the driver on its tick cadence
    /// (§9.3). Never a completion signal by itself — `AttemptEnded` is.
    Tick {
        progress: Vec<(AttemptId, u64)>,
    },
    AttemptEnded {
        id: AttemptId,
        outcome: AttemptOutcome,
        /// The attempt's final durable position in the file, whether it
        /// succeeded or failed — durable because §8 step 4 only advances
        /// this after the positional write already landed.
        position: u64,
    },
    ProbeDone {
        file: FileId,
        result: Result<ProbeResult, ProbeError>,
    },
    FilePrepared {
        file: FileId,
        result: Result<super::effect::PreparedFile, StorageError>,
    },
    CheckpointDone {
        package: PackageId,
        result: Result<(), StorageError>,
    },
    /// A fresh interface scan (P3.4): the complete set of usable links
    /// now. The driver sends it once at startup and again whenever the
    /// link monitor sees the set change.
    LinksChanged(Vec<Link>),
}
