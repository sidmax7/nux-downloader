//! The types that cross the IPC boundary. They are flat, display-ready
//! views of the engine's own types (which stay free of IPC concerns).
//! Sizes are `u64` in Rust and `number` in TypeScript; a file would have
//! to exceed 8 PiB to lose precision.

use crate::settings::Settings;
use nuxd_core::engine::{EngineSnapshot, LinkStats};
use nuxd_core::model::{FileEntry, FileStatus, LinkKind, Package, PackageStatus, Priority};
use nuxd_core::net::health::LinkState;
use nuxd_core::{PackageDraft, collect::DraftFile};
use serde::{Deserialize, Serialize};
use specta::Type;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum PriorityDto {
    Low,
    Normal,
    High,
}

impl From<Priority> for PriorityDto {
    fn from(priority: Priority) -> Self {
        match priority {
            Priority::Low => Self::Low,
            Priority::Normal => Self::Normal,
            Priority::High => Self::High,
        }
    }
}

impl From<PriorityDto> for Priority {
    fn from(priority: PriorityDto) -> Self {
        match priority {
            PriorityDto::Low => Self::Low,
            PriorityDto::Normal => Self::Normal,
            PriorityDto::High => Self::High,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum PackageState {
    Queued,
    Downloading,
    Paused,
    Verifying,
    Extracting,
    Done,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum FileState {
    Unprobed,
    Queued,
    Preparing,
    Active,
    Complete,
    Verified,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct FileDto {
    pub id: u64,
    pub name: String,
    pub state: FileState,
    pub error: Option<String>,
    pub size: Option<u64>,
    pub downloaded: u64,
    pub blocks: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct PackageDto {
    pub id: u64,
    pub name: String,
    pub dest_dir: String,
    pub priority: PriorityDto,
    pub state: PackageState,
    /// Why the package failed, or why it paused on its own (disk full…).
    pub error: Option<String>,
    pub extract: bool,
    pub delete_archives: bool,
    /// Whether the package looks like an archive, so the two extraction
    /// options apply to it.
    pub is_archive: bool,
    pub limit: Option<u64>,
    /// `None` while any file's size is still unknown.
    pub size: Option<u64>,
    pub downloaded: u64,
    pub files: Vec<FileDto>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum LinkKindDto {
    Wifi,
    Ethernet,
    Usb,
    Bridge,
    Other,
}

impl From<LinkKind> for LinkKindDto {
    fn from(kind: LinkKind) -> Self {
        match kind {
            LinkKind::Wifi => Self::Wifi,
            LinkKind::Ethernet => Self::Ethernet,
            LinkKind::UsbTether => Self::Usb,
            LinkKind::Bridge => Self::Bridge,
            LinkKind::Other => Self::Other,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct LinkDto {
    /// The OS interface name — the key for settings and commands.
    pub name: String,
    pub id: u64,
    /// The user's name for it, else the system's label.
    pub label: String,
    pub color: Option<String>,
    pub kind: LinkKindDto,
    pub address: Option<String>,
    pub enabled: bool,
    /// Taken out of rotation after repeated connection failures.
    pub degraded: bool,
    pub active: u32,
    pub bytes: u64,
    /// Bytes per second.
    pub speed: u64,
    pub limit: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct SnapshotDto {
    pub packages: Vec<PackageDto>,
    pub links: Vec<LinkDto>,
    pub global_limit: Option<u64>,
    /// Set when the engine could not start (another `nuxd` holds the
    /// data directory, say); everything else is then empty.
    pub engine_error: Option<String>,
}

impl SnapshotDto {
    #[must_use]
    pub fn failed(message: String) -> Self {
        Self {
            packages: Vec::new(),
            links: Vec::new(),
            global_limit: None,
            engine_error: Some(message),
        }
    }
}

/// One block of a file for the grid: how much of it is secured, and which
/// link delivered it.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct BlockDto {
    /// 0–100.
    pub done: u8,
    pub link: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct BlocksDto {
    pub blocks: Vec<BlockDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct DraftFileDto {
    pub source: String,
    pub name: Option<String>,
    pub size: Option<u64>,
    pub error: Option<String>,
}

/// A package waiting for review (`collect` → `enqueue`).
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct DraftDto {
    /// Pass back to `enqueue_draft`.
    pub id: u64,
    pub name: String,
    pub files: Vec<DraftFileDto>,
    /// Looks like an archive: offer "extract after download".
    pub offers_extract: bool,
    pub missing_volumes: Vec<u32>,
    pub ok_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct EnqueueRequest {
    pub draft_id: u64,
    pub name: String,
    /// `None` = the download folder from the settings.
    pub dest_dir: Option<String>,
    pub priority: PriorityDto,
    pub extract: bool,
    pub delete_archives: bool,
}

/// Sent now and then so the UI can show a toast.
#[derive(Debug, Clone, Serialize, Deserialize, Type, tauri_specta::Event)]
pub struct NoticeEvent {
    pub level: NoticeLevel,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum NoticeLevel {
    Info,
    Error,
}

/// The whole picture, pushed at up to 4 Hz and after any change.
#[derive(Debug, Clone, Serialize, Deserialize, Type, tauri_specta::Event)]
pub struct SnapshotEvent(pub SnapshotDto);

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct SettingsResult {
    pub settings: Settings,
    /// Some changed settings only apply when the engine restarts.
    pub restart_needed: bool,
}

fn block_len(layout_block: u64, index: u32, size: Option<u64>) -> u64 {
    match size {
        Some(size) => {
            let start = u64::from(index).saturating_mul(layout_block);
            layout_block.min(size.saturating_sub(start))
        }
        None => u64::MAX,
    }
}

/// Bytes of a file that are secured on disk.
fn downloaded(file: &FileEntry) -> u64 {
    if matches!(file.status, FileStatus::Complete | FileStatus::Verified) {
        return file
            .expected_size
            .unwrap_or_else(|| file.parts.iter().map(|part| part.durable).sum());
    }
    let Some(layout) = file.layout else {
        return 0;
    };
    file.parts
        .iter()
        .enumerate()
        .map(|(index, part)| {
            let len = if layout.is_single_open_ended() {
                u64::MAX
            } else {
                block_len(layout.block_size, index as u32, file.expected_size)
            };
            part.durable.min(len)
        })
        .sum()
}

fn file_state(status: &FileStatus) -> (FileState, Option<String>) {
    match status {
        FileStatus::Unprobed => (FileState::Unprobed, None),
        FileStatus::Queued => (FileState::Queued, None),
        FileStatus::Preparing => (FileState::Preparing, None),
        FileStatus::Active => (FileState::Active, None),
        FileStatus::Complete => (FileState::Complete, None),
        FileStatus::Verified => (FileState::Verified, None),
        FileStatus::Failed(error) => (FileState::Failed, Some(error.to_string())),
    }
}

impl From<&FileEntry> for FileDto {
    fn from(file: &FileEntry) -> Self {
        let (state, error) = file_state(&file.status);
        Self {
            id: file.id.get(),
            name: file.name.clone(),
            state,
            error,
            size: file.expected_size,
            downloaded: downloaded(file),
            blocks: file.layout.map_or(0, |layout| layout.count),
        }
    }
}

impl From<&Package> for PackageDto {
    fn from(package: &Package) -> Self {
        let (state, status_error) = match &package.status {
            PackageStatus::Queued => (PackageState::Queued, None),
            PackageStatus::Downloading => (PackageState::Downloading, None),
            PackageStatus::Paused => (PackageState::Paused, None),
            PackageStatus::Verifying => (PackageState::Verifying, None),
            PackageStatus::Extracting => (PackageState::Extracting, None),
            PackageStatus::Done => (PackageState::Done, None),
            PackageStatus::Failed(error) => (PackageState::Failed, Some(error.to_string())),
        };
        let files: Vec<FileDto> = package.files.iter().map(FileDto::from).collect();
        let size = files
            .iter()
            .map(|file| file.size)
            .try_fold(0u64, |total, size| size.map(|size| total + size));
        Self {
            id: package.id.get(),
            name: package.name.clone(),
            dest_dir: package.dest_dir.display().to_string(),
            priority: package.priority.into(),
            state,
            error: status_error.or_else(|| package.last_error.as_ref().map(ToString::to_string)),
            extract: package.extract,
            delete_archives: package.delete_archives,
            is_archive: package.archive.is_some(),
            limit: package.limit.map(nuxd_core::model::ByteRate::bytes_per_sec),
            size,
            downloaded: files.iter().map(|file| file.downloaded).sum(),
            files,
        }
    }
}

/// The block grid of one file, or `None` when there is no such file.
#[must_use]
pub fn blocks_of(file: &FileEntry) -> BlocksDto {
    let Some(layout) = file.layout else {
        return BlocksDto { blocks: Vec::new() };
    };
    let finished = matches!(file.status, FileStatus::Complete | FileStatus::Verified);
    let blocks = file
        .parts
        .iter()
        .enumerate()
        .map(|(index, part)| {
            let done = if finished {
                100
            } else if layout.is_single_open_ended() {
                u8::from(part.durable > 0) * 50
            } else {
                let len = block_len(layout.block_size, index as u32, file.expected_size).max(1);
                (part.durable.min(len) * 100 / len) as u8
            };
            BlockDto {
                done,
                link: part.link.map(|link| link.get()),
            }
        })
        .collect();
    BlocksDto { blocks }
}

impl From<&DraftFile> for DraftFileDto {
    fn from(file: &DraftFile) -> Self {
        match &file.probe {
            Ok(probe) => Self {
                source: file.source.clone(),
                name: Some(probe.suggested_name.clone()),
                size: probe.expected_size,
                error: None,
            },
            Err(error) => Self {
                source: file.source.clone(),
                name: None,
                size: None,
                error: Some(error.clone()),
            },
        }
    }
}

#[must_use]
pub fn draft_dto(id: u64, draft: &PackageDraft) -> DraftDto {
    DraftDto {
        id,
        name: draft.name.clone(),
        files: draft.files.iter().map(DraftFileDto::from).collect(),
        offers_extract: draft.offers_extract(),
        missing_volumes: draft.missing_volumes.clone(),
        ok_count: draft.ok_count() as u32,
    }
}

/// A link as the UI sees it, with the user's preferences applied.
#[must_use]
pub fn link_dto(stats: &LinkStats, settings: &Settings, limit: Option<u64>) -> LinkDto {
    let link = &stats.link;
    let pref = settings.link_prefs.get(&link.name);
    LinkDto {
        name: link.name.clone(),
        id: link.id.get(),
        label: pref
            .and_then(|p| p.label.clone())
            .filter(|label| !label.trim().is_empty())
            .unwrap_or_else(|| link.label.clone()),
        color: pref.and_then(|p| p.color.clone()),
        kind: link.kind.into(),
        address: link
            .v4
            .map(|ip| ip.to_string())
            .or_else(|| link.v6.map(|ip| ip.to_string())),
        enabled: true,
        degraded: matches!(stats.state, LinkState::Degraded { .. }),
        active: stats.active,
        bytes: stats.bytes,
        speed: stats.speed.max(0.0).round() as u64,
        limit,
    }
}

/// The snapshot the UI renders. `remembered` are links the user switched
/// off (they are not in the engine's set), shown as disabled.
#[must_use]
pub fn snapshot_dto(
    snapshot: &EngineSnapshot,
    settings: &Settings,
    global_limit: Option<u64>,
    limit_of: impl Fn(&LinkStats) -> Option<u64>,
    remembered: &[LinkDto],
) -> SnapshotDto {
    let mut links: Vec<LinkDto> = snapshot
        .links
        .iter()
        .map(|stats| link_dto(stats, settings, limit_of(stats)))
        .collect();
    for old in remembered {
        let off = settings
            .link_prefs
            .get(&old.name)
            .is_some_and(|pref| !pref.enabled);
        if off && !links.iter().any(|link| link.name == old.name) {
            let mut link = old.clone();
            link.enabled = false;
            link.degraded = false;
            link.active = 0;
            link.speed = 0;
            links.push(link);
        }
    }
    links.sort_by(|a, b| a.name.cmp(&b.name));
    SnapshotDto {
        packages: snapshot.packages.iter().map(PackageDto::from).collect(),
        links,
        global_limit,
        engine_error: None,
    }
}
