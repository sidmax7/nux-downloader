//! `EngineState`: the pure functional core (`ARCHITECTURE.md` §14). All
//! download logic — planning, scheduling, retries, checkpoint cadence,
//! completion — lives here, and only here. No I/O, no clock reads
//! (`now` is always a `handle` argument), no channels. The driver
//! (P2.7) is the only thing that ever touches a socket or a file.

use super::effect::{Effect, EngineEvent, FilePrep};
use super::input::{AbortReason, AttemptOutcome, Command, Input};
use crate::config::EngineConfig;
use crate::error::{Blame, blame};
use crate::model::{
    AttemptId, FileEntry, FileError, FileId, FileStatus, Link, LinkId, Package, PackageError,
    PackageId, PackageStatus, PartStatus,
};
use crate::net::health::{LinkHealth, LinkState};
use crate::plan;
use crate::sched;
use crate::storage::manifest::{PackageManifest, QueueManifest};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use url::Url;

/// Runtime-only per-block bookkeeping (§4: "never persisted"): retry
/// count and backoff, the avoid-rule's per-link zero-delivery flags, and
/// which attempt (if any) currently holds the block.
#[derive(Debug, Clone, Default)]
struct BlockRuntime {
    retries: u32,
    retry_until: Option<Instant>,
    avoid_links: HashSet<LinkId>,
    in_flight: Option<AttemptId>,
}

/// Runtime-only record of one in-flight attempt — just enough to route
/// `Tick` samples and `AttemptEnded` back to the right file and block.
#[derive(Debug, Clone, Copy)]
struct AttemptState {
    package: PackageId,
    file: FileId,
    block: u32,
    link: LinkId,
    /// The absolute file offset this attempt's request started at —
    /// `position - range_start` is how many bytes *this attempt itself*
    /// delivered, for the avoid rule.
    range_start: u64,
}

/// Smoothed delivery rate of one link, sampled on ticks.
#[derive(Debug, Clone, Copy, Default)]
struct Meter {
    last_bytes: u64,
    last_at: Option<Instant>,
    speed: f64,
}

#[derive(Debug, Clone, Copy, Default)]
struct CheckpointState {
    bytes_since: u64,
    last: Option<Instant>,
}

/// The engine's pure state: everything in `ARCHITECTURE.md` §4's
/// persisted domain model, plus the runtime-only bookkeeping §4 calls out
/// as never persisted.
#[derive(Debug)]
pub struct EngineState {
    config: EngineConfig,
    packages: Vec<Package>,
    package_index: HashMap<PackageId, usize>,
    file_index: HashMap<FileId, (PackageId, usize)>,
    links: Vec<Link>,
    /// Circuit-breaker state per link (§6.4); absent means healthy.
    health: HashMap<LinkId, LinkHealth>,
    /// Durable bytes credited to each link since this engine started.
    link_bytes: HashMap<LinkId, u64>,
    meters: HashMap<LinkId, Meter>,
    attempts: HashMap<AttemptId, AttemptState>,
    blocks: HashMap<(FileId, u32), BlockRuntime>,
    checkpoints: HashMap<PackageId, CheckpointState>,
    next_package_id: u64,
    next_file_id: u64,
    next_attempt_id: u64,
}

impl EngineState {
    #[must_use]
    pub fn new(config: EngineConfig) -> Self {
        Self {
            config,
            packages: Vec::new(),
            package_index: HashMap::new(),
            file_index: HashMap::new(),
            links: Vec::new(),
            health: HashMap::new(),
            link_bytes: HashMap::new(),
            meters: HashMap::new(),
            attempts: HashMap::new(),
            blocks: HashMap::new(),
            checkpoints: HashMap::new(),
            next_package_id: 1,
            next_file_id: 1,
            next_attempt_id: 1,
        }
    }

    #[must_use]
    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    #[must_use]
    pub fn packages(&self) -> &[Package] {
        &self.packages
    }

    #[must_use]
    pub fn package(&self, id: PackageId) -> Option<&Package> {
        self.package_ref(id)
    }

    #[must_use]
    pub fn file(&self, id: FileId) -> Option<&FileEntry> {
        let &(package_id, index) = self.file_index.get(&id)?;
        self.package_ref(package_id)?.files.get(index)
    }

    #[must_use]
    pub fn links(&self) -> &[Link] {
        &self.links
    }

    #[must_use]
    pub fn attempt_count(&self) -> usize {
        self.attempts.len()
    }

    /// A link's circuit-breaker state (§6.4).
    #[must_use]
    pub fn link_state(&self, id: LinkId) -> LinkState {
        self.health
            .get(&id)
            .map_or(LinkState::Up, LinkHealth::state)
    }

    /// Durable bytes credited to `id` since this engine started.
    #[must_use]
    pub fn link_bytes(&self, id: LinkId) -> u64 {
        self.link_bytes.get(&id).copied().unwrap_or(0)
    }

    /// Test-only peeks at scheduler bookkeeping the public API doesn't
    /// expose.
    #[cfg(test)]
    pub(crate) fn attempt_link(&self, id: AttemptId) -> LinkId {
        self.attempts
            .get(&id)
            .unwrap_or_else(|| panic!("no attempt {id:?}"))
            .link
    }

    #[cfg(test)]
    pub(crate) fn block_retries(&self, file: FileId, block: u32) -> u32 {
        self.blocks.get(&(file, block)).map_or(0, |b| b.retries)
    }

    /// The id counters as of right now — persisted in `queue.json` so a
    /// restart never reuses an id a completed or removed package's
    /// leftover state might still reference (§9.4).
    #[must_use]
    pub fn counters(&self) -> (u64, u64) {
        (self.next_package_id, self.next_file_id)
    }

    #[must_use]
    pub fn snapshot(&self) -> super::EngineSnapshot {
        super::EngineSnapshot {
            packages: self.packages.clone(),
            next_package_id: self.next_package_id,
            next_file_id: self.next_file_id,
            links: self
                .links
                .iter()
                .map(|link| super::LinkStats {
                    link: link.clone(),
                    state: self.link_state(link.id),
                    active: u32::try_from(
                        self.attempts.values().filter(|a| a.link == link.id).count(),
                    )
                    .unwrap_or(u32::MAX),
                    bytes: self.link_bytes(link.id),
                    speed: self.meters.get(&link.id).map_or(0.0, |m| m.speed),
                })
                .collect(),
        }
    }

    /// Rebuilds engine state from persisted manifests on startup (§9.4).
    /// Returns the state plus the effects needed to make it live again:
    /// re-probing anything that never finished activating, and reopening
    /// (not recreating) the `.nuxdpart` of anything that was already
    /// `Active`. A package that was `Downloading` stays that way unless
    /// `config.auto_resume_on_startup` is off, in which case it comes
    /// back `Paused` — either way, reopening a file costs nothing but a
    /// local file handle, so that happens regardless of a package's pause
    /// state; only `fill_slots`, gated on `PackageStatus::Downloading`,
    /// actually starts moving bytes.
    #[must_use]
    pub fn restore(
        config: EngineConfig,
        queue: &QueueManifest,
        mut packages: Vec<Package>,
    ) -> (Self, Vec<Effect>) {
        let mut state = Self::new(config);
        state.next_package_id = queue.next_package_id;
        state.next_file_id = queue.next_file_id;

        // `queue.package_order` is authoritative for queue position;
        // anything not listed in it (shouldn't happen, but a manifest
        // directory can be edited by hand) sorts after everything that
        // is, in whatever order it was found on disk.
        packages.sort_by_key(|package| {
            queue
                .package_order
                .iter()
                .position(|id| *id == package.id)
                .unwrap_or(usize::MAX)
        });

        let mut effects = Vec::new();
        for package in &mut packages {
            if !state.config.auto_resume_on_startup && package.status == PackageStatus::Downloading
            {
                package.status = PackageStatus::Paused;
            }
            for file in &mut package.files {
                match file.status {
                    FileStatus::Active => {
                        file.status = FileStatus::Preparing;
                        effects.push(Effect::PrepareFile {
                            file: file.id,
                            plan: FilePrep {
                                package: package.id,
                                dest_dir: package.dest_dir.clone(),
                                suggested_name: file.name.clone(),
                                expected_size: file.expected_size,
                                resume_final_name: Some(file.name.clone()),
                            },
                        });
                    }
                    FileStatus::Preparing | FileStatus::Queued => {
                        // Died before finishing activation last time —
                        // there is nothing on disk yet to reopen, and a
                        // stale probe result is no cheaper to keep around
                        // than a fresh one, so this just restarts the
                        // file's whole lifecycle instead of resuming it.
                        let source_url = file.url.clone().or_else(|| Url::parse(&file.source).ok());
                        file.status = FileStatus::Unprobed;
                        file.url = None;
                        file.layout = None;
                        file.parts.clear();
                        if let Some(url) = source_url {
                            effects.push(Effect::Probe {
                                file: file.id,
                                url,
                                via: None,
                            });
                        }
                    }
                    FileStatus::Unprobed => {
                        if let Ok(url) = Url::parse(&file.source) {
                            effects.push(Effect::Probe {
                                file: file.id,
                                url,
                                via: None,
                            });
                        }
                    }
                    FileStatus::Complete | FileStatus::Verified | FileStatus::Failed(_) => {}
                }
            }
        }

        for (index, package) in packages.into_iter().enumerate() {
            state.package_index.insert(package.id, index);
            for (file_index, file) in package.files.iter().enumerate() {
                state.file_index.insert(file.id, (package.id, file_index));
            }
            state.packages.push(package);
        }

        (state, effects)
    }

    /// The single entry point: folds one `Input` into the state and
    /// returns the effects the driver should execute. Every state change
    /// happens inside this call — see `ARCHITECTURE.md` §14.1.
    pub fn handle(&mut self, input: Input, now: Instant) -> Vec<Effect> {
        let mut effects = Vec::new();
        match input {
            Input::Command(command) => self.handle_command(command, now, &mut effects),
            Input::Tick { progress } => self.handle_tick(progress, now, &mut effects),
            Input::AttemptEnded {
                id,
                outcome,
                position,
            } => self.handle_attempt_ended(id, outcome, position, now, &mut effects),
            Input::ProbeDone { file, result } => self.handle_probe_done(file, result, &mut effects),
            Input::FilePrepared { file, result } => {
                self.handle_file_prepared(file, result, now, &mut effects);
            }
            Input::CheckpointDone { package, result } => {
                self.handle_checkpoint_done(package, result, &mut effects);
            }
            Input::LinksChanged(links) => self.handle_links_changed(links, &mut effects),
        }
        self.fill_slots(now, &mut effects);
        effects
    }

    /// Folds a fresh link scan in (§6.1). An attempt on a link that
    /// vanished — or whose addresses changed, which breaks a connection
    /// bound to the old ones — is aborted with `LinkLost`. Its block goes
    /// straight back to pending: no retry is counted and the link isn't
    /// added to the block's avoid set, because the link did nothing
    /// wrong that a healthy link wouldn't repeat. New links get slots
    /// from the `fill_slots` pass that ends every `handle` call.
    fn handle_links_changed(&mut self, links: Vec<Link>, effects: &mut Vec<Effect>) {
        if links == self.links {
            return;
        }
        let lost: Vec<AttemptId> = self
            .attempts
            .iter()
            .filter(|(_, attempt)| {
                let before = self.links.iter().find(|l| l.id == attempt.link);
                let after = links.iter().find(|l| l.id == attempt.link);
                match (before, after) {
                    (_, None) => true,
                    (Some(before), Some(after)) => before.v4 != after.v4 || before.v6 != after.v6,
                    (None, Some(_)) => false,
                }
            })
            .map(|(id, _)| *id)
            .collect();
        for id in lost {
            if let Some(attempt) = self.attempts.remove(&id) {
                if let Some(block) = self.blocks.get_mut(&(attempt.file, attempt.block)) {
                    block.in_flight = None;
                }
                effects.push(Effect::AbortAttempt {
                    id,
                    reason: AbortReason::LinkLost,
                });
            }
        }
        self.health
            .retain(|id, _| links.iter().any(|link| link.id == *id));
        self.meters
            .retain(|id, _| links.iter().any(|link| link.id == *id));
        self.links = links;
        effects.push(Effect::Emit(EngineEvent::LinksChanged));
    }

    // --- id allocation (§9.4: "the engine owns the counters") ---

    fn alloc_package_id(&mut self) -> PackageId {
        let id = PackageId::new(self.next_package_id);
        self.next_package_id += 1;
        id
    }

    fn alloc_file_id(&mut self) -> FileId {
        let id = FileId::new(self.next_file_id);
        self.next_file_id += 1;
        id
    }

    fn alloc_attempt_id(&mut self) -> AttemptId {
        let id = AttemptId::new(self.next_attempt_id);
        self.next_attempt_id += 1;
        id
    }

    // --- lookups ---

    fn package_mut(&mut self, id: PackageId) -> Option<&mut Package> {
        let &index = self.package_index.get(&id)?;
        self.packages.get_mut(index)
    }

    fn package_ref(&self, id: PackageId) -> Option<&Package> {
        let &index = self.package_index.get(&id)?;
        self.packages.get(index)
    }

    // --- commands ---

    fn handle_command(&mut self, command: Command, now: Instant, effects: &mut Vec<Effect>) {
        match command {
            Command::AddPackage {
                name,
                dest_dir,
                sources,
                priority,
            } => self.add_package(name, dest_dir, sources, priority, effects),
            Command::Pause(package) => self.pause_package(package, now, effects),
            Command::Resume(package) => self.resume_package(package, effects),
            Command::Cancel(package) => self.cancel_package(package, effects),
            Command::Remove(package) => self.remove_package(package, effects),
            Command::Shutdown => self.checkpoint_all(now, effects),
        }
    }

    fn add_package(
        &mut self,
        name: String,
        dest_dir: std::path::PathBuf,
        sources: Vec<String>,
        priority: crate::model::Priority,
        effects: &mut Vec<Effect>,
    ) {
        let package_id = self.alloc_package_id();
        let mut files = Vec::new();
        let mut urls: Vec<Option<Url>> = Vec::new();
        for source in sources {
            let file_id = self.alloc_file_id();
            match Url::parse(&source) {
                Ok(url) => {
                    let placeholder_name = crate::probe::filename_from_url(&url);
                    let Some(file) = FileEntry::new(file_id, source.clone(), placeholder_name)
                    else {
                        continue;
                    };
                    files.push(file);
                    urls.push(Some(url));
                }
                Err(_) => {
                    let Some(mut file) = FileEntry::new(file_id, source.clone(), "download") else {
                        continue;
                    };
                    file.status = FileStatus::Failed(FileError::ProbeFailed {
                        detail: "couldn't parse as a URL".to_string(),
                    });
                    files.push(file);
                    urls.push(None);
                }
            }
        }
        if files.is_empty() {
            return;
        }

        let Some(mut package) = Package::new(package_id, name, dest_dir, files) else {
            return;
        };
        package.priority = priority;
        package.status = PackageStatus::Downloading;

        let index = self.packages.len();
        for (file_index, file) in package.files.iter().enumerate() {
            self.file_index.insert(file.id, (package_id, file_index));
        }
        for (file, url) in package.files.iter().zip(urls.iter()) {
            if let Some(url) = url {
                effects.push(Effect::Probe {
                    file: file.id,
                    url: url.clone(),
                    via: None,
                });
            }
        }
        self.package_index.insert(package_id, index);
        self.packages.push(package);
        effects.push(Effect::Emit(EngineEvent::PackageAdded {
            package: package_id,
        }));
        self.update_package_status(package_id, effects);
    }

    fn pause_package(&mut self, package_id: PackageId, now: Instant, effects: &mut Vec<Effect>) {
        let Some(package) = self.package_mut(package_id) else {
            return;
        };
        if !matches!(package.status, PackageStatus::Downloading) {
            return;
        }
        package.status = PackageStatus::Paused;
        self.abort_package_attempts(package_id, AbortReason::Pause, effects);
        // Checkpoint before announcing the pause, so a subscriber that
        // reacts to `PackagePaused` can trust the checkpoint has already
        // landed (§9.3) — the driver executes effects strictly in order.
        self.checkpoint_now(package_id, now, effects);
        effects.push(Effect::Emit(EngineEvent::PackagePaused {
            package: package_id,
        }));
    }

    fn resume_package(&mut self, package_id: PackageId, effects: &mut Vec<Effect>) {
        let Some(package) = self.package_mut(package_id) else {
            return;
        };
        if package.status != PackageStatus::Paused {
            return;
        }
        package.status = PackageStatus::Downloading;
        effects.push(Effect::Emit(EngineEvent::PackageResumed {
            package: package_id,
        }));
    }

    fn cancel_package(&mut self, package_id: PackageId, effects: &mut Vec<Effect>) {
        self.abort_package_attempts(package_id, AbortReason::Cancel, effects);
        let Some(package) = self.package_mut(package_id) else {
            return;
        };
        let error = PackageError::Other {
            detail: "cancelled".to_string(),
        };
        package.status = PackageStatus::Failed(error.clone());
        effects.push(Effect::Emit(EngineEvent::PackageFailed {
            package: package_id,
            error,
        }));
    }

    fn remove_package(&mut self, package_id: PackageId, effects: &mut Vec<Effect>) {
        self.abort_package_attempts(package_id, AbortReason::Cancel, effects);
        let Some(&index) = self.package_index.get(&package_id) else {
            return;
        };
        let package = self.packages.remove(index);
        self.package_index.remove(&package_id);
        self.checkpoints.remove(&package_id);
        for file in &package.files {
            self.file_index.remove(&file.id);
            self.blocks.retain(|(file_id, _), _| *file_id != file.id);
        }
        // Everything after the removed package shifted down one slot.
        for (new_index, later) in self.packages.iter().enumerate().skip(index) {
            self.package_index.insert(later.id, new_index);
            for (file_index, file) in later.files.iter().enumerate() {
                self.file_index.insert(file.id, (later.id, file_index));
            }
        }
        effects.push(Effect::Emit(EngineEvent::PackageRemoved {
            package: package_id,
        }));
    }

    fn checkpoint_all(&mut self, now: Instant, effects: &mut Vec<Effect>) {
        let package_ids: Vec<PackageId> = self.packages.iter().map(|p| p.id).collect();
        for id in package_ids {
            if let Some(package) = self.package_ref(id) {
                effects.push(Effect::Checkpoint {
                    package: id,
                    manifest: PackageManifest::new(package.clone()),
                });
            }
            let checkpoint = self.checkpoints.entry(id).or_default();
            checkpoint.bytes_since = 0;
            checkpoint.last = Some(now);
        }
    }

    fn abort_package_attempts(
        &mut self,
        package_id: PackageId,
        reason: AbortReason,
        effects: &mut Vec<Effect>,
    ) {
        let ids: Vec<AttemptId> = self
            .attempts
            .iter()
            .filter(|(_, attempt)| attempt.package == package_id)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if let Some(attempt) = self.attempts.remove(&id) {
                if let Some(block) = self.blocks.get_mut(&(attempt.file, attempt.block)) {
                    block.in_flight = None;
                }
                effects.push(Effect::AbortAttempt { id, reason });
            }
        }
    }

    // --- probe / prepare lifecycle (§7.3) ---

    fn handle_probe_done(
        &mut self,
        file_id: FileId,
        result: Result<crate::probe::ProbeResult, crate::error::ProbeError>,
        effects: &mut Vec<Effect>,
    ) {
        let Some(&(package_id, index)) = self.file_index.get(&file_id) else {
            return;
        };
        match result {
            Ok(probe) => {
                let layout = plan::plan_layout_with(
                    probe.expected_size,
                    probe.supports_ranges,
                    &self.config,
                );
                let Some(package) = self.package_mut(package_id) else {
                    return;
                };
                let file = &mut package.files[index];
                file.url = Some(probe.final_url);
                file.name = probe.suggested_name;
                file.expected_size = probe.expected_size;
                file.supports_ranges = probe.supports_ranges;
                file.versions = vec![probe.version];
                file.layout = Some(layout);
                file.parts = vec![PartStatus::not_started(); layout.count as usize];
                file.status = FileStatus::Preparing;

                let plan = FilePrep {
                    package: package_id,
                    dest_dir: package.dest_dir.clone(),
                    suggested_name: file.name.clone(),
                    expected_size: file.expected_size,
                    resume_final_name: None,
                };
                effects.push(Effect::Emit(EngineEvent::FileProbed { file: file_id }));
                effects.push(Effect::PrepareFile {
                    file: file_id,
                    plan,
                });
            }
            Err(error) => {
                self.fail_file(
                    package_id,
                    index,
                    FileError::ProbeFailed {
                        detail: error.to_string(),
                    },
                    effects,
                );
                self.update_package_status(package_id, effects);
            }
        }
    }

    fn handle_file_prepared(
        &mut self,
        file_id: FileId,
        result: Result<super::effect::PreparedFile, crate::error::StorageError>,
        now: Instant,
        effects: &mut Vec<Effect>,
    ) {
        let Some(&(package_id, index)) = self.file_index.get(&file_id) else {
            return;
        };
        match result {
            Ok(prepared) => {
                if let Some(package) = self.package_mut(package_id) {
                    let file = &mut package.files[index];
                    file.name = prepared.final_name;
                    file.status = FileStatus::Active;
                    if !prepared.frontiers_valid {
                        // §9.4: the resumed `.nuxdpart` didn't match what
                        // the manifest claimed, so none of its durable
                        // progress can be trusted — every block restarts.
                        if let Some(layout) = file.layout {
                            file.parts = vec![PartStatus::not_started(); layout.count as usize];
                        }
                    }
                }
                effects.push(Effect::Emit(EngineEvent::FileActive { file: file_id }));
                // An empty file (a single durable-by-construction zero-length
                // block) has no bytes to request — nothing will ever start
                // or end an attempt for it, so completion has to be checked
                // right here rather than waiting for one. The checkpoint
                // goes into `effects` before `post_effects` is appended, so
                // a subscriber that reacts to `FileVerified`/`PackageDone`
                // can trust the checkpoint has already landed (§9.3) — the
                // driver executes effects strictly in order.
                let mut post_effects = Vec::new();
                let completed = self.try_complete_file(package_id, index, false, &mut post_effects);
                self.update_package_status(package_id, &mut post_effects);
                if completed {
                    self.checkpoint_now(package_id, now, effects);
                }
                effects.extend(post_effects);
            }
            Err(error) => {
                self.fail_file(
                    package_id,
                    index,
                    FileError::Io {
                        detail: error.to_string(),
                    },
                    effects,
                );
                self.fail_package_fatally(package_id, error.to_string(), effects);
            }
        }
    }

    fn handle_checkpoint_done(
        &mut self,
        package_id: PackageId,
        result: Result<(), crate::error::StorageError>,
        effects: &mut Vec<Effect>,
    ) {
        if let Err(error) = result {
            self.fail_package_fatally(package_id, error.to_string(), effects);
        }
    }

    fn fail_package_fatally(
        &mut self,
        package_id: PackageId,
        detail: String,
        effects: &mut Vec<Effect>,
    ) {
        if let Some(package) = self.package_mut(package_id) {
            package.status = PackageStatus::Failed(PackageError::Io {
                detail: detail.clone(),
            });
        } else {
            return;
        }
        self.abort_package_attempts(package_id, AbortReason::Fatal, effects);
        effects.push(Effect::Emit(EngineEvent::PackageFailed {
            package: package_id,
            error: PackageError::Io { detail },
        }));
    }

    fn fail_file(
        &mut self,
        package_id: PackageId,
        index: usize,
        error: FileError,
        effects: &mut Vec<Effect>,
    ) {
        let Some(package) = self.package_mut(package_id) else {
            return;
        };
        package.files[index].status = FileStatus::Failed(error.clone());
        let file_id = package.files[index].id;
        effects.push(Effect::Emit(EngineEvent::FileFailed {
            file: file_id,
            error,
        }));
    }

    // --- progress and completion ---

    /// Advances a block's durable frontier from an absolute file
    /// position — shared by `Tick` sampling and `AttemptEnded`'s final
    /// position, since both report the same durable-write guarantee
    /// (§8 step 4, §9.3).
    fn apply_position(
        &mut self,
        package_id: PackageId,
        index: usize,
        block: u32,
        link: LinkId,
        position: u64,
    ) {
        let Some(package) = self.package_mut(package_id) else {
            return;
        };
        let file = &mut package.files[index];
        let Some(layout) = file.layout else {
            return;
        };
        let total = file.expected_size.unwrap_or(0);
        let Some(block_range) = plan::block_range(&layout, total, block) else {
            return;
        };
        if position < block_range.start {
            return;
        }
        let delta_from_block_start = position - block_range.start;
        let Some(part) = file.parts.get_mut(block as usize) else {
            return;
        };
        if delta_from_block_start > part.durable {
            let delta = delta_from_block_start - part.durable;
            part.durable = delta_from_block_start;
            part.link = Some(link);
            self.checkpoints.entry(package_id).or_default().bytes_since += delta;
            // Bytes arrived over this link: it works (§6.4).
            self.health.entry(link).or_default().record_success();
            *self.link_bytes.entry(link).or_default() += delta;
        }
    }

    fn handle_tick(
        &mut self,
        progress: Vec<(AttemptId, u64)>,
        now: Instant,
        effects: &mut Vec<Effect>,
    ) {
        let mut touched: HashSet<PackageId> = HashSet::new();
        for (attempt_id, position) in progress {
            let Some(attempt) = self.attempts.get(&attempt_id).copied() else {
                continue;
            };
            let Some(&(package_id, index)) = self.file_index.get(&attempt.file) else {
                continue;
            };
            self.apply_position(package_id, index, attempt.block, attempt.link, position);
            touched.insert(package_id);
        }
        self.update_meters(now);
        for package_id in &touched {
            self.maybe_checkpoint(*package_id, now, effects);
        }
        let due: Vec<PackageId> = self
            .checkpoints
            .keys()
            .copied()
            .filter(|id| !touched.contains(id))
            .collect();
        for package_id in due {
            self.maybe_checkpoint(package_id, now, effects);
        }
    }

    /// Folds each link's byte total into its smoothed speed.
    fn update_meters(&mut self, now: Instant) {
        const SMOOTHING: f64 = 0.3;
        for link in &self.links {
            let bytes = self.link_bytes.get(&link.id).copied().unwrap_or(0);
            let meter = self.meters.entry(link.id).or_default();
            if let Some(last_at) = meter.last_at {
                let secs = now.duration_since(last_at).as_secs_f64();
                if secs > 0.0 {
                    let instant = bytes.saturating_sub(meter.last_bytes) as f64 / secs;
                    meter.speed += SMOOTHING * (instant - meter.speed);
                }
            }
            meter.last_bytes = bytes;
            meter.last_at = Some(now);
        }
    }

    fn maybe_checkpoint(&mut self, package_id: PackageId, now: Instant, effects: &mut Vec<Effect>) {
        let Some(checkpoint) = self.checkpoints.get(&package_id) else {
            return;
        };
        let due_by_time = checkpoint
            .last
            .is_none_or(|last| now.duration_since(last) >= self.config.checkpoint_interval);
        let due_by_bytes = checkpoint.bytes_since >= self.config.checkpoint_bytes_threshold;
        if !due_by_time && !due_by_bytes {
            return;
        }
        let Some(package) = self.package_ref(package_id) else {
            return;
        };
        effects.push(Effect::Checkpoint {
            package: package_id,
            manifest: PackageManifest::new(package.clone()),
        });
        let checkpoint = self.checkpoints.entry(package_id).or_default();
        checkpoint.bytes_since = 0;
        checkpoint.last = Some(now);
    }

    fn handle_attempt_ended(
        &mut self,
        id: AttemptId,
        outcome: AttemptOutcome,
        position: u64,
        now: Instant,
        effects: &mut Vec<Effect>,
    ) {
        let Some(attempt) = self.attempts.remove(&id) else {
            return;
        };
        let Some(&(package_id, index)) = self.file_index.get(&attempt.file) else {
            return;
        };

        self.apply_position(package_id, index, attempt.block, attempt.link, position);

        let delivered = position.saturating_sub(attempt.range_start);
        let block_key = (attempt.file, attempt.block);
        {
            let block = self.blocks.entry(block_key).or_default();
            block.in_flight = None;
            if delivered == 0 {
                block.avoid_links.insert(attempt.link);
            } else {
                block.avoid_links.remove(&attempt.link);
            }
        }

        let is_success = matches!(outcome, AttemptOutcome::Success);
        let is_open_ended = self
            .package_ref(package_id)
            .and_then(|package| package.files[index].layout)
            .is_some_and(|layout| layout.is_single_open_ended());

        match outcome {
            AttemptOutcome::Success => {
                if let Some(block) = self.blocks.get_mut(&block_key) {
                    block.retries = 0;
                    block.retry_until = None;
                }
            }
            AttemptOutcome::Failed(error) => {
                if blame(&error) == Blame::Link {
                    let health = self.health.entry(attempt.link).or_default();
                    if let Some(until) = health.record_failure(now, &self.config) {
                        effects.push(Effect::WakeAt(until));
                    }
                }
                self.handle_attempt_failure(package_id, index, block_key, &error, now, effects);
            }
        }

        // A bounded file is complete once every block is durable, however
        // its last attempt ended — one that failed `Overlong` after
        // writing its whole range still finished the block. An open-ended
        // file has no length to compare against, so only a clean end of
        // body (success) can finish it.
        // The checkpoint goes into `effects` before `post_effects` is
        // appended, so a subscriber reacting to `FileVerified`/
        // `PackageDone` can trust the checkpoint already landed (§9.3) —
        // the driver executes effects strictly in order.
        let mut post_effects = Vec::new();
        let completed = self.try_complete_file(
            package_id,
            index,
            is_open_ended && is_success,
            &mut post_effects,
        );
        self.update_package_status(package_id, &mut post_effects);
        if completed {
            self.checkpoint_now(package_id, now, effects);
        }
        effects.extend(post_effects);
    }

    /// An immediate checkpoint, outside the tick cadence (§9.3: "on pause,
    /// file completion, and shutdown").
    fn checkpoint_now(&mut self, package_id: PackageId, now: Instant, effects: &mut Vec<Effect>) {
        let Some(package) = self.package_ref(package_id) else {
            return;
        };
        effects.push(Effect::Checkpoint {
            package: package_id,
            manifest: PackageManifest::new(package.clone()),
        });
        let checkpoint = self.checkpoints.entry(package_id).or_default();
        checkpoint.bytes_since = 0;
        checkpoint.last = Some(now);
    }

    fn handle_attempt_failure(
        &mut self,
        package_id: PackageId,
        index: usize,
        block_key: (FileId, u32),
        error: &crate::error::TransferError,
        now: Instant,
        effects: &mut Vec<Effect>,
    ) {
        match blame(error) {
            // §16: the block retries at once and uncounted — the link,
            // not the block, is what's suspect. The link's circuit
            // breaker (§6.4) is what stops a dead link from being tried
            // forever.
            Blame::Link => {}
            Blame::Attempt | Blame::Host => {
                let retries = {
                    let block = self.blocks.entry(block_key).or_default();
                    block.retries += 1;
                    block.retries
                };
                if retries > self.config.max_retries_per_block {
                    self.fail_file(package_id, index, FileError::TooManyRetries, effects);
                } else {
                    let backoff = retry_backoff(&self.config, retries);
                    if let Some(block) = self.blocks.get_mut(&block_key) {
                        block.retry_until = Some(now + backoff);
                    }
                    effects.push(Effect::WakeAt(now + backoff));
                }
            }
            Blame::File => {
                self.fail_file(
                    package_id,
                    index,
                    FileError::Other {
                        detail: error.to_string(),
                    },
                    effects,
                );
            }
            Blame::Fatal => {
                self.fail_file(
                    package_id,
                    index,
                    FileError::Io {
                        detail: error.to_string(),
                    },
                    effects,
                );
                self.fail_package_fatally(package_id, error.to_string(), effects);
            }
        }
    }

    /// Marks the file `Verified` and asks for it to be finalized if every
    /// byte is durable. `open_ended_finished` is the only completion
    /// signal an unknown-size file has: its single attempt reached a clean
    /// end of body. Returns whether the file completed.
    fn try_complete_file(
        &mut self,
        package_id: PackageId,
        index: usize,
        open_ended_finished: bool,
        effects: &mut Vec<Effect>,
    ) -> bool {
        let Some(package) = self.package_mut(package_id) else {
            return false;
        };
        let file = &mut package.files[index];
        if file.status != FileStatus::Active {
            return false;
        }
        let Some(layout) = file.layout else {
            return false;
        };

        let complete = if layout.is_single_open_ended() {
            open_ended_finished
        } else {
            let total = file.expected_size.unwrap_or(0);
            (0..layout.count).all(|i| match plan::block_len(&layout, total, i) {
                Some(len) => file
                    .parts
                    .get(i as usize)
                    .is_some_and(|part| part.durable >= len),
                None => false,
            })
        };
        if !complete {
            return false;
        }

        if layout.is_single_open_ended()
            && let Some(part) = file.parts.first()
        {
            file.expected_size = Some(part.durable);
        }

        // M1 has no checksum to validate against (§12.3 lands with
        // verification in Phase 5); durability plus an exact size is the
        // whole of "verified" for now.
        file.status = FileStatus::Verified;
        let file_id = file.id;
        effects.push(Effect::Emit(EngineEvent::FileVerified { file: file_id }));
        effects.push(Effect::Finalize { file: file_id });
        true
    }

    fn update_package_status(&mut self, package_id: PackageId, effects: &mut Vec<Effect>) {
        let Some(package) = self.package_mut(package_id) else {
            return;
        };
        if matches!(
            package.status,
            PackageStatus::Done | PackageStatus::Failed(_)
        ) {
            return;
        }
        if package.all_files_verified() {
            package.status = PackageStatus::Done;
            effects.push(Effect::Emit(EngineEvent::PackageDone {
                package: package_id,
            }));
            return;
        }
        let all_settled = package
            .files
            .iter()
            .all(|file| matches!(file.status, FileStatus::Verified | FileStatus::Failed(_)));
        if all_settled {
            package.status = PackageStatus::Failed(PackageError::FilesFailed);
            effects.push(Effect::Emit(EngineEvent::PackageFailed {
                package: package_id,
                error: PackageError::FilesFailed,
            }));
        }
    }

    // --- scheduling (§7.2) ---

    fn build_candidates(&self, now: Instant) -> Vec<sched::FileCandidate> {
        let mut out = Vec::new();
        for (queue_position, package) in self.packages.iter().enumerate() {
            if package.status != PackageStatus::Downloading {
                continue;
            }
            for file in &package.files {
                if file.status != FileStatus::Active {
                    continue;
                }
                let Some(layout) = file.layout else { continue };
                let Some(url) = &file.url else { continue };
                let Some(host) = url.host_str() else { continue };
                let total = file.expected_size.unwrap_or(0);

                let mut blocks = Vec::new();
                for block_index in 0..layout.count {
                    let Some(range) = plan::block_range(&layout, total, block_index) else {
                        continue;
                    };
                    let durable = file
                        .parts
                        .get(block_index as usize)
                        .map_or(0, |part| part.durable);
                    if let Some(len) = plan::block_len(&layout, total, block_index)
                        && durable >= len
                    {
                        continue;
                    }
                    let runtime = self.blocks.get(&(file.id, block_index));
                    if runtime.is_some_and(|block| block.in_flight.is_some()) {
                        continue;
                    }
                    let retry_ok = runtime
                        .and_then(|block| block.retry_until)
                        .is_none_or(|until| now >= until);
                    if !retry_ok {
                        continue;
                    }
                    let avoid_links = runtime
                        .map(|block| block.avoid_links.iter().copied().collect())
                        .unwrap_or_default();
                    // A server without range support can only send the file
                    // from byte 0, so a retry restarts there; the bytes it
                    // re-sends over the durable prefix are identical, and a
                    // frontier never moves backwards (`apply_position`).
                    let durable = if file.supports_ranges { durable } else { 0 };
                    blocks.push(sched::CandidateBlock {
                        index: block_index,
                        range,
                        durable,
                        avoid_links,
                    });
                }
                if blocks.is_empty() {
                    continue;
                }
                out.push(sched::FileCandidate {
                    file: file.id,
                    host: host.to_string(),
                    priority: package.priority,
                    queue_position: u64::try_from(queue_position).unwrap_or(u64::MAX),
                    blocks,
                });
            }
        }
        out
    }

    fn active_attempt_hosts(&self) -> HashMap<AttemptId, String> {
        self.attempts
            .iter()
            .filter_map(|(id, attempt)| {
                let &(package_id, index) = self.file_index.get(&attempt.file)?;
                let host = self.package_ref(package_id)?.files[index]
                    .url
                    .as_ref()?
                    .host_str()?;
                Some((*id, host.to_string()))
            })
            .collect()
    }

    /// Fills every free slot on every link (§7.2), one attempt per link
    /// per round so the links' first blocks interleave instead of the
    /// first link taking everything — called at the end of every
    /// `handle` call so the engine stays work-conserving regardless of
    /// which input triggered it.
    fn fill_slots(&mut self, now: Instant, effects: &mut Vec<Effect>) {
        let links = self.links.clone();
        loop {
            let mut started = false;
            for link in &links {
                started |= self.start_one_on(link, now, effects);
            }
            if !started {
                break;
            }
        }
    }

    /// Starts at most one attempt on `link`, if it has a free slot (its
    /// own cap, narrowed by its health) and the scheduler has work for
    /// it. Returns whether one was started.
    fn start_one_on(&mut self, link: &Link, now: Instant, effects: &mut Vec<Effect>) -> bool {
        let cap = self
            .health
            .get(&link.id)
            .map_or(u32::from(link.max_slots), |health| {
                health.slot_cap(u32::from(link.max_slots), now)
            });
        let active_on_link = u32::try_from(
            self.attempts
                .values()
                .filter(|attempt| attempt.link == link.id)
                .count(),
        )
        .unwrap_or(u32::MAX);
        if active_on_link >= cap {
            return false;
        }
        if u32::try_from(self.attempts.len()).unwrap_or(u32::MAX)
            >= self.config.max_connections_total
        {
            return false;
        }

        let candidates = self.build_candidates(now);
        if candidates.is_empty() {
            return false;
        }
        let host_map = self.active_attempt_hosts();
        let active: Vec<sched::ActiveAttempt<'_>> = self
            .attempts
            .iter()
            .filter_map(|(id, attempt)| {
                let host = host_map.get(id)?;
                Some(sched::ActiveAttempt {
                    file: attempt.file,
                    link: attempt.link,
                    host: host.as_str(),
                })
            })
            .collect();
        let view = sched::SchedulerView {
            files: &candidates,
            active: &active,
            hosts: &self.config.hosts,
            config: &self.config,
        };
        let Some(work) = sched::pick_work(&view, link.id) else {
            return false;
        };
        let sched::Work::Primary {
            file,
            block,
            range,
            file_offset,
        } = work;
        let Some(&(package_id, index)) = self.file_index.get(&file) else {
            return false;
        };
        let Some(url) = self
            .package_ref(package_id)
            .and_then(|p| p.files[index].url.clone())
        else {
            return false;
        };
        let id = self.alloc_attempt_id();
        self.attempts.insert(
            id,
            AttemptState {
                package: package_id,
                file,
                block,
                link: link.id,
                range_start: range.start,
            },
        );
        self.blocks.entry((file, block)).or_default().in_flight = Some(id);
        effects.push(Effect::StartAttempt {
            id,
            file,
            url,
            range,
            file_offset,
            link: link.id,
        });
        true
    }
}

/// Retry backoff (§7.6): `min(base × 2^(n-1), cap)`. The exponent is
/// clamped well before `2^exponent` could overflow a `u32` shift — the
/// cap always wins long before that matters.
fn retry_backoff(config: &EngineConfig, attempt_n: u32) -> Duration {
    let exponent = attempt_n.saturating_sub(1).min(16);
    let scaled = config.retry_base_backoff.saturating_mul(1u32 << exponent);
    scaled.min(config.retry_max_backoff)
}
