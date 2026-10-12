//! The engine's domain model (`ARCHITECTURE.md` §4). Data types only —
//! no I/O, no async. `version.rs` and `plan.rs` hold the pure logic that
//! operates on `FileVersion` and `BlockLayout` respectively.

mod byte;
mod file;
mod ids;
mod link;
mod package;
mod priority;

pub use byte::{ByteRange, ByteRate};
pub use file::{BlockLayout, FileEntry, FileError, FileStatus, FileVersion, PartStatus};
pub use ids::{AttemptId, FileId, LinkId, PackageId};
pub use link::{Link, LinkKind};
pub use package::{ArchiveScheme, ArchiveSet, Package, PackageError, PackageStatus, PostOptions};
pub use priority::Priority;
