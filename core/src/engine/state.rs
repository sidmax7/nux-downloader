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
    PackageId, PackageStatus, PartStatus, PostOptions,
};
use crate::net::health::{LinkHealth, LinkState};
use crate::plan;
use crate::sched;
use crate::storage::manifest::{PackageManifest, QueueManifest};
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};
use url::Url;

/// Runtime-only per-block bookkeeping (§4: "never persisted"): retry
/// count and backoff, the avoid-rule's per-link zero-delivery flags, the
/// attempts running on the block (the holder, plus a hedge while one
/// races it, §7.5) and how many hedges it has had.
#[derive(Debug, Clone, Default)]
struct BlockRuntime {
    retries: u32,
    retry_until: Option<Instant>,
    avoid_links: HashSet<LinkId>,
    in_flight: Vec<AttemptId>,
    hedges: u32,
    /// Times the slow-connection refresh restarted an attempt on this
    /// block (§7.5); capped by `max_refreshes_per_block`.
    refreshes: u32,
}

/// Runtime-only record of one in-flight attempt — just enough to route
/// `Tick` samples and `AttemptEnded` back to the right file and block.
#[derive(Debug, Clone)]
struct AttemptState {
    package: PackageId,
    file: FileId,
    block: u32,
    link: LinkId,
    /// The absolute file offset this attempt's request started at —
    /// `position - range_start` is how many bytes *this attempt itself*
    /// delivered, for the avoid rule.
    range_start: u64,
    /// When it started and how far it has got (from ticks), for hedging's
    /// age and ETA (§7.5).
    started_at: Instant,
    position: u64,
    /// When `position` last advanced, and recent `(time, position)`
    /// samples taken on ticks — the refresh's evidence (§7.5). Positions
    /// move in write-buffer steps, so speed is judged over a window of
    /// `refresh_slow_for`, never between two ticks.
    last_advance: Instant,
    samples: std::collections::VecDeque<(Instant, u64)>,
}

impl AttemptState {
    /// Records a tick sample, keeping one anchor at or before `span` ago.
    fn sample(&mut self, now: Instant, span: Duration) {
        self.samples.push_back((now, self.position));
        while self.samples.len() >= 2
            && self
                .samples
                .get(1)
                .is_some_and(|(at, _)| now.saturating_duration_since(*at) >= span)
        {
            self.samples.pop_front();
        }
    }

    /// Bytes/s over the last `span`, or `None` until a full span is on
    /// record.
    fn window_speed(&self, now: Instant, span: Duration) -> Option<f64> {
        let (at, position) = *self.samples.front()?;
        let covered = now.saturating_duration_since(at);
        if covered < span || covered.is_zero() {
            return None;
        }
        Some(self.position.saturating_sub(position) as f64 / covered.as_secs_f64())
    }
}

/// Smoothed delivery rate of one link, sampled on ticks.
#[derive(Debug, Clone, Copy, Default)]
struct Meter {
    last_bytes: u64,
    last_at: Option<Instant>,
    speed: f64,
}

/// What the engine has learned about one host from `429`/`503`
/// responses (§16: "Host backoff; no retry counted"), per link: hosts
/// limit each client IP, and every link is its own IP (§7.1), so one
/// link being refused says nothing about another.
#[derive(Debug, Clone, Copy, Default)]
struct HostState {
    /// No new connections to the host before this.
    cooldown_until: Option<Instant>,
    /// Consecutive refusals with no bytes delivered in between.
    strikes: u32,
    /// Connections the host has shown it will tolerate: halved on each
    /// refusal, raised by one for every attempt that then delivers.
    /// `None` = no restriction learned.
    learned_cap: Option<u32>,
}

/// A host that refuses this many times running, with nothing delivered,
/// fails the file instead of retrying forever.
const MAX_HOST_STRIKES: u32 = 10;
/// Host backoff after the first refusal, doubling per strike to the cap.
const HOST_BACKOFF_BASE: Duration = Duration::from_secs(2);
const HOST_BACKOFF_MAX: Duration = Duration::from_secs(60);
/// The longest `Retry-After` honored as given; longer ones are clamped.
const RETRY_AFTER_MAX: Duration = Duration::from_secs(600);
/// A `429`/`503` on a file's probe is retried this many times.
const MAX_PROBE_RETRIES: u32 = 6;

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
    /// Keyed by (host, link); `None` is the unbound probe client.
    hosts: HashMap<(String, Option<LinkId>), HostState>,
    /// Files whose probe was refused with `429`/`503`: when to try again,
    /// and how many tries so far.
    probe_waits: HashMap<FileId, (Instant, u32)>,
    /// Files with a probe request in flight (the probe pool, §5).
    probing: HashSet<FileId>,
    /// What each package's manifest last held, so a checkpoint can tell a
    /// clean package (nothing to write) from a dirty one.
    written: HashMap<PackageId, Package>,
    /// Set for management-only sessions (`restore_offline`): nothing is
    /// probed.
    idle: bool,
    /// Files whose source is being re-resolved after a `403`/`410`;
    /// nothing new starts for them until it reports back.
    refreshing: HashSet<FileId>,
    /// Verified packages waiting for the extraction pool (concurrency 1,
    /// §12.4), in the order they became ready.
    extract_queue: VecDeque<PackageId>,
    /// The package being extracted now.
    extracting: Option<PackageId>,
    /// Files that already used their one re-resolve (§13: "once").
    reresolved: HashSet<FileId>,
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
            hosts: HashMap::new(),
            probe_waits: HashMap::new(),
            probing: HashSet::new(),
            written: HashMap::new(),
            idle: false,
            refreshing: HashSet::new(),
            extract_queue: VecDeque::new(),
            extracting: None,
            reresolved: HashSet::new(),
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
        packages: Vec<Package>,
    ) -> (Self, Vec<Effect>) {
        Self::restore_inner(config, queue, packages, false)
    }

    /// Loads persisted state **as it is**, for management commands
    /// (`nuxd add`, `pause`, `queue`, …) that must not start, reopen or
    /// re-probe anything — and must not rewrite what they didn't touch.
    /// The state is idle: no probes are issued and, with no links, no
    /// transfers start. Commands still apply and checkpoint.
    #[must_use]
    pub fn restore_offline(
        config: EngineConfig,
        queue: &QueueManifest,
        packages: Vec<Package>,
    ) -> Self {
        Self::restore_inner(config, queue, packages, true).0
    }

    fn restore_inner(
        config: EngineConfig,
        queue: &QueueManifest,
        mut packages: Vec<Package>,
        offline: bool,
    ) -> (Self, Vec<Effect>) {
        let mut state = Self::new(config);
        state.idle = offline;
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
        for package in packages.iter_mut().filter(|_| !offline) {
            if !state.config.auto_resume_on_startup && package.status == PackageStatus::Downloading
            {
                package.status = PackageStatus::Paused;
            }
            // Interrupted post-processing starts over from verification.
            if matches!(
                package.status,
                PackageStatus::Verifying | PackageStatus::Extracting
            ) {
                package.status = PackageStatus::Downloading;
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
                        file.status = FileStatus::Unprobed;
                        file.url = None;
                        file.layout = None;
                        file.parts.clear();
                        // The probe pool picks it up (`pump_probes`).
                    }
                    FileStatus::Unprobed => {}
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
        if !offline {
            let ids: Vec<PackageId> = state.packages.iter().map(|p| p.id).collect();
            for id in ids {
                if state
                    .package_ref(id)
                    .is_some_and(|p| p.status == PackageStatus::Downloading)
                {
                    state.update_package_status(id, &mut effects);
                }
            }
            state.pump_extract(&mut effects);
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
            Input::VersionAccepted { file, version } => {
                if let Some(&(package_id, index)) = self.file_index.get(&file)
                    && let Some(package) = self.package_mut(package_id)
                    && !package.files[index].versions.contains(&version)
                {
                    package.files[index].versions.push(version);
                }
            }
            Input::Reresolved { file, result } => {
                self.handle_reresolved(file, result, &mut effects);
            }
            Input::AttemptEnded {
                id,
                outcome,
                position,
            } => self.handle_attempt_ended(id, outcome, position, now, &mut effects),
            Input::ProbeDone { file, result } => {
                self.handle_probe_done(file, result, now, &mut effects);
            }
            Input::FilePrepared { file, result } => {
                self.handle_file_prepared(file, result, now, &mut effects);
            }
            Input::CheckpointDone { package, result } => {
                self.handle_checkpoint_done(package, result, &mut effects);
            }
            Input::VerifyDone { package, result } => {
                self.handle_verify_done(package, result, &mut effects);
            }
            Input::ExtractDone { package, result } => {
                self.handle_extract_done(package, result, &mut effects);
            }
            Input::LinksChanged(links) => self.handle_links_changed(links, &mut effects),
        }
        self.pump_probes(now, &mut effects);
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
                    block.in_flight.retain(|a| *a != id);
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
                post,
            } => self.add_package(
                NewPackage {
                    name,
                    dest_dir,
                    priority,
                    post,
                },
                sources,
                now,
                effects,
            ),
            Command::Pause(package) => self.pause_package(package, now, effects),
            Command::AddProbed {
                name,
                dest_dir,
                files,
                priority,
                post,
            } => self.add_probed(
                NewPackage {
                    name,
                    dest_dir,
                    priority,
                    post,
                },
                files,
                now,
                effects,
            ),
            Command::SetPackageLimit { package, limit } => {
                self.set_package_limit(package, limit, now, effects);
            }
            Command::SetPostOptions {
                package,
                extract,
                delete_archives,
            } => self.set_post_options(package, extract, delete_archives, now, effects),
            Command::RestartFile(file) => self.restart_file(file, now, effects),
            Command::SetPriority { package, priority } => {
                self.set_priority(package, priority, now, effects);
            }
            Command::MoveTo { package, position } => self.move_package(package, position, effects),
            Command::Resume(package) => self.resume_package(package, now, effects),
            Command::Cancel(package) => self.cancel_package(package, now, effects),
            Command::Remove(package) => self.remove_package(package, effects),
            Command::Shutdown => {
                // A management session changed only what its commands
                // already checkpointed; rewriting the rest is churn.
                if !self.idle {
                    self.checkpoint_all(now, effects);
                }
            }
        }
    }

    fn add_package(
        &mut self,
        new: NewPackage,
        sources: Vec<String>,
        now: Instant,
        effects: &mut Vec<Effect>,
    ) {
        let NewPackage {
            name,
            dest_dir,
            priority,
            post,
        } = new;
        let package_id = self.alloc_package_id();
        let mut files = Vec::new();
        for source in sources {
            let file_id = self.alloc_file_id();
            match Url::parse(&source) {
                Ok(url) if !crate::resolve::is_supported_scheme(&url) => {
                    let Some(mut file) = FileEntry::new(file_id, source.clone(), "download") else {
                        continue;
                    };
                    file.status = FileStatus::Failed(FileError::ProbeFailed {
                        detail: format!("unsupported scheme `{}`", url.scheme()),
                    });
                    files.push(file);
                }
                Ok(url) => {
                    let placeholder_name = crate::probe::filename_from_url(&url);
                    let Some(file) = FileEntry::new(file_id, source.clone(), placeholder_name)
                    else {
                        continue;
                    };
                    files.push(file);
                    let _ = url;
                }
                Err(_) => {
                    let Some(mut file) = FileEntry::new(file_id, source.clone(), "download") else {
                        continue;
                    };
                    file.status = FileStatus::Failed(FileError::ProbeFailed {
                        detail: "couldn't parse as a URL".to_string(),
                    });
                    files.push(file);
                }
            }
        }
        let Some(mut package) = Package::new(package_id, name, dest_dir, files) else {
            return;
        };
        apply_post(&mut package, post);
        self.install_package(package, priority, now, effects);
    }

    /// Puts a freshly built package at the back of the queue, `Downloading`.
    fn install_package(
        &mut self,
        mut package: Package,
        priority: crate::model::Priority,
        now: Instant,
        effects: &mut Vec<Effect>,
    ) {
        let package_id = package.id;
        package.priority = priority;
        package.status = PackageStatus::Downloading;

        let index = self.packages.len();
        for (file_index, file) in package.files.iter().enumerate() {
            self.file_index.insert(file.id, (package_id, file_index));
        }
        self.package_index.insert(package_id, index);
        self.packages.push(package);
        // A new package is durable at once, not at the next tick — but
        // the periodic cadence stays fresh, so its first progress is
        // checkpointed promptly rather than a full interval later.
        self.checkpoint_now(package_id, now, effects);
        if let Some(checkpoint) = self.checkpoints.get_mut(&package_id) {
            checkpoint.last = None;
        }
        effects.push(Effect::Emit(EngineEvent::PackageAdded {
            package: package_id,
        }));
        self.update_package_status(package_id, effects);
    }

    /// A `403`/`410` on a file: signed URLs expire, so re-resolve its
    /// source once (§13). Returns whether the failure is absorbed — true
    /// while a re-resolve is running or was just started, false once the
    /// file has already had its one.
    fn begin_reresolve(
        &mut self,
        package_id: PackageId,
        index: usize,
        file: FileId,
        effects: &mut Vec<Effect>,
    ) -> bool {
        if self.refreshing.contains(&file) {
            return true;
        }
        if self.reresolved.contains(&file) {
            return false;
        }
        let Some(source) = self
            .package_ref(package_id)
            .map(|p| p.files[index].source.clone())
        else {
            return false;
        };
        self.reresolved.insert(file);
        self.refreshing.insert(file);
        effects.push(Effect::Reresolve { file, source });
        true
    }

    fn handle_reresolved(
        &mut self,
        file_id: FileId,
        result: Result<crate::probe::ProbeResult, String>,
        effects: &mut Vec<Effect>,
    ) {
        if !self.refreshing.remove(&file_id) {
            return;
        }
        let Some(&(package_id, index)) = self.file_index.get(&file_id) else {
            return;
        };
        match result {
            Ok(probe) => {
                let expected = self
                    .package_ref(package_id)
                    .and_then(|p| p.files[index].expected_size);
                if probe.expected_size != expected {
                    self.fail_file(
                        package_id,
                        index,
                        FileError::RemoteChanged {
                            detail: format!(
                                "size changed from {expected:?} to {:?} while refreshing the link",
                                probe.expected_size
                            ),
                        },
                        effects,
                    );
                    self.update_package_status(package_id, effects);
                } else if let Some(package) = self.package_mut(package_id) {
                    package.files[index].url = Some(probe.final_url);
                }
            }
            Err(detail) => {
                self.fail_file(
                    package_id,
                    index,
                    FileError::ProbeFailed {
                        detail: format!("couldn't refresh the link: {detail}"),
                    },
                    effects,
                );
                self.update_package_status(package_id, effects);
            }
        }
    }

    /// `AddProbed`: like `add_package`, but each file arrives already
    /// probed, so it goes straight to `Queued` (§7.3).
    fn add_probed(
        &mut self,
        new: NewPackage,
        inputs: Vec<super::input::ProbedInput>,
        now: Instant,
        effects: &mut Vec<Effect>,
    ) {
        let NewPackage {
            name,
            dest_dir,
            priority,
            post,
        } = new;
        let package_id = self.alloc_package_id();
        let mut files = Vec::new();
        for input in inputs {
            let file_id = self.alloc_file_id();
            let placeholder = Url::parse(&input.source)
                .map(|url| crate::probe::filename_from_url(&url))
                .unwrap_or_else(|_| "download".to_string());
            let Some(mut file) = FileEntry::new(file_id, input.source, placeholder) else {
                continue;
            };
            match input.probe {
                Ok(probe) => self.apply_probe(&mut file, probe),
                Err(detail) => {
                    file.status = FileStatus::Failed(FileError::ProbeFailed { detail });
                }
            }
            files.push(file);
        }
        let Some(mut package) = Package::new(package_id, name, dest_dir, files) else {
            return;
        };
        apply_post(&mut package, post);
        self.install_package(package, priority, now, effects);
    }

    /// Records what a probe found on `file` and leaves it `Queued`:
    /// probed and planned, but not activated — no placeholder file or
    /// descriptor exists until the scheduler wants it (§7.3).
    fn apply_probe(&self, file: &mut FileEntry, probe: crate::probe::ProbeResult) {
        let layout =
            plan::plan_layout_with(probe.expected_size, probe.supports_ranges, &self.config);
        file.url = Some(probe.final_url);
        file.name = probe.suggested_name;
        file.expected_size = probe.expected_size;
        file.supports_ranges = probe.supports_ranges;
        file.versions = vec![probe.version];
        file.layout = Some(layout);
        file.parts = vec![PartStatus::not_started(); layout.count as usize];
        file.status = FileStatus::Queued;
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

    fn resume_package(&mut self, package_id: PackageId, now: Instant, effects: &mut Vec<Effect>) {
        let Some(package) = self.package_mut(package_id) else {
            return;
        };
        // A package whose verification or extraction failed retries that
        // step; its files are all there.
        if matches!(
            package.status,
            PackageStatus::Failed(
                PackageError::VerifyFailed { .. } | PackageError::ExtractFailed { .. }
            )
        ) && package.all_files_verified()
        {
            package.status = PackageStatus::Downloading;
            self.update_package_status(package_id, effects);
            self.pump_extract(effects);
            return;
        }
        if package.status != PackageStatus::Paused {
            return;
        }
        package.status = PackageStatus::Downloading;
        // Persist the status change now: a management session may exit
        // before any tick.
        self.checkpoint_now(package_id, now, effects);
        effects.push(Effect::Emit(EngineEvent::PackageResumed {
            package: package_id,
        }));
    }

    fn cancel_package(&mut self, package_id: PackageId, now: Instant, effects: &mut Vec<Effect>) {
        self.abort_package_attempts(package_id, AbortReason::Cancel, effects);
        self.stop_post(package_id, effects);
        let Some(package) = self.package_mut(package_id) else {
            return;
        };
        let error = PackageError::Other {
            detail: "cancelled".to_string(),
        };
        package.status = PackageStatus::Failed(error.clone());
        self.checkpoint_now(package_id, now, effects);
        effects.push(Effect::Emit(EngineEvent::PackageFailed {
            package: package_id,
            error,
        }));
    }

    fn set_priority(
        &mut self,
        package_id: PackageId,
        priority: crate::model::Priority,
        now: Instant,
        effects: &mut Vec<Effect>,
    ) {
        let Some(package) = self.package_mut(package_id) else {
            return;
        };
        if package.priority == priority {
            return;
        }
        package.priority = priority;
        // The priority lives in the package's own manifest, so it is
        // checkpointed at once rather than waiting for the next tick;
        // the event follows so a subscriber can trust it landed.
        self.checkpoint_now(package_id, now, effects);
        effects.push(Effect::Emit(EngineEvent::PackagePriorityChanged {
            package: package_id,
        }));
    }

    fn move_package(&mut self, package_id: PackageId, position: usize, effects: &mut Vec<Effect>) {
        let Some(&from) = self.package_index.get(&package_id) else {
            return;
        };
        let to = position.min(self.packages.len() - 1);
        if from == to {
            return;
        }
        let package = self.packages.remove(from);
        self.packages.insert(to, package);
        for (index, package) in self.packages.iter().enumerate() {
            self.package_index.insert(package.id, index);
        }
        effects.push(Effect::Emit(EngineEvent::QueueReordered));
    }

    fn remove_package(&mut self, package_id: PackageId, effects: &mut Vec<Effect>) {
        self.abort_package_attempts(package_id, AbortReason::Cancel, effects);
        self.stop_post(package_id, effects);
        let Some(&index) = self.package_index.get(&package_id) else {
            return;
        };
        let package = self.packages.remove(index);
        self.package_index.remove(&package_id);
        self.checkpoints.remove(&package_id);
        self.written.remove(&package_id);
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
            self.write_checkpoint(id, false, effects);
            self.mark_checkpointed(id, now);
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
                    block.in_flight.retain(|a| *a != id);
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
        now: Instant,
        effects: &mut Vec<Effect>,
    ) {
        self.probing.remove(&file_id);
        let Some(&(package_id, index)) = self.file_index.get(&file_id) else {
            return;
        };
        match result {
            Ok(probe) => {
                self.probe_waits.remove(&file_id);
                let mut entry = match self.package_ref(package_id) {
                    Some(package) => package.files[index].clone(),
                    None => return,
                };
                self.apply_probe(&mut entry, probe);
                if let Some(package) = self.package_mut(package_id) {
                    package.files[index] = entry;
                }
                effects.push(Effect::Emit(EngineEvent::FileProbed { file: file_id }));
            }
            Err(error) => {
                // A host that answers `429`/`503` is busy, not wrong:
                // wait out its backoff and probe again.
                let busy = match &error {
                    crate::error::ProbeError::Busy { retry_after, .. } => Some(*retry_after),
                    crate::error::ProbeError::UnexpectedStatus { status: 429 | 503 } => Some(None),
                    _ => None,
                };
                if let Some(retry_after) = busy
                    && self.schedule_reprobe(file_id, package_id, index, retry_after, now, effects)
                {
                    return;
                }
                self.probe_waits.remove(&file_id);
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

    /// Puts a refused probe on a timer, penalizing its host. `false` if
    /// the retries are used up (the caller then fails the file).
    fn schedule_reprobe(
        &mut self,
        file_id: FileId,
        package_id: PackageId,
        index: usize,
        retry_after: Option<Duration>,
        now: Instant,
        effects: &mut Vec<Effect>,
    ) -> bool {
        let tries = self.probe_waits.get(&file_id).map_or(0, |(_, n)| *n) + 1;
        if tries > MAX_PROBE_RETRIES {
            return false;
        }
        let Some(host) = self
            .package_ref(package_id)
            .and_then(|p| Url::parse(&p.files[index].source).ok())
            .and_then(|url| url.host_str().map(str::to_string))
        else {
            return false;
        };
        let until = self.penalize_host(&host, None, retry_after, now, effects);
        self.probe_waits.insert(file_id, (until, tries));
        true
    }

    /// The probe pool (§5): issues probes for `Unprobed` files, in queue
    /// order, without ever exceeding `max_concurrent_probes` in flight
    /// overall or `max_probes_per_host` to one host. A file whose probe
    /// was refused (`429`/`503`) waits out its host's backoff first.
    fn pump_probes(&mut self, now: Instant, effects: &mut Vec<Effect>) {
        if self.idle {
            return;
        }
        let total_cap = self.config.max_concurrent_probes as usize;
        if self.probing.len() >= total_cap {
            return;
        }
        let per_host_cap = self.config.max_probes_per_host;
        let host_of = |source: &str| {
            Url::parse(source)
                .ok()
                .and_then(|url| url.host_str().map(str::to_string))
        };
        let mut per_host: HashMap<String, u32> = HashMap::new();
        for file_id in &self.probing {
            if let Some(&(package_id, index)) = self.file_index.get(file_id)
                && let Some(package) = self.package_ref(package_id)
                && let Some(host) = host_of(&package.files[index].source)
            {
                *per_host.entry(host).or_default() += 1;
            }
        }

        let mut order: Vec<usize> = (0..self.packages.len()).collect();
        order.sort_by(|&a, &b| {
            self.packages[b]
                .priority
                .cmp(&self.packages[a].priority)
                .then(a.cmp(&b))
        });
        let mut to_probe = Vec::new();
        'outer: for package_index in order {
            let package = &self.packages[package_index];
            if package.status != PackageStatus::Downloading {
                continue;
            }
            for file in &package.files {
                if file.status != FileStatus::Unprobed || self.probing.contains(&file.id) {
                    continue;
                }
                if self
                    .probe_waits
                    .get(&file.id)
                    .is_some_and(|(until, _)| now < *until)
                {
                    continue;
                }
                let Ok(url) = Url::parse(&file.source) else {
                    continue;
                };
                let Some(host) = url.host_str().map(str::to_string) else {
                    continue;
                };
                let count = per_host.entry(host).or_default();
                if *count >= per_host_cap {
                    continue;
                }
                *count += 1;
                to_probe.push((file.id, url));
                if self.probing.len() + to_probe.len() >= total_cap {
                    break 'outer;
                }
            }
        }
        for (file, url) in to_probe {
            self.probing.insert(file);
            effects.push(Effect::Probe {
                file,
                url,
                via: None,
            });
        }
    }

    /// Bytes came from this file's host: it is not refusing us, so the
    /// strike count resets and the learned connection cap creeps back up.
    fn host_delivered(&mut self, package_id: PackageId, index: usize, link: LinkId) {
        let Some(host) = self
            .package_ref(package_id)
            .and_then(|p| p.files[index].url.as_ref())
            .and_then(|url| url.host_str().map(str::to_string))
        else {
            return;
        };
        let Some(state) = self.hosts.get_mut(&(host, Some(link))) else {
            return;
        };
        state.strikes = 0;
        state.learned_cap = state.learned_cap.map(|cap| cap + 1);
    }

    /// A transfer on `link` was refused by the file's host.
    fn host_refused(
        &mut self,
        package_id: PackageId,
        index: usize,
        link: LinkId,
        retry_after: Option<Duration>,
        now: Instant,
        effects: &mut Vec<Effect>,
    ) {
        let Some(host) = self
            .package_ref(package_id)
            .and_then(|p| p.files[index].url.as_ref())
            .and_then(|url| url.host_str().map(str::to_string))
        else {
            return;
        };
        self.penalize_host(&host, Some(link), retry_after, now, effects);
        let strikes = self.hosts.get(&(host, Some(link))).map_or(0, |h| h.strikes);
        if strikes > MAX_HOST_STRIKES {
            self.fail_file(package_id, index, FileError::TooManyRetries, effects);
        }
    }

    /// A host refused us (`429`/`503`): back off, and learn a lower
    /// connection cap. Returns when it may be tried again.
    fn penalize_host(
        &mut self,
        host: &str,
        link: Option<LinkId>,
        retry_after: Option<Duration>,
        now: Instant,
        effects: &mut Vec<Effect>,
    ) -> Instant {
        // The host's own `Retry-After`, when given, is a floor under our
        // backoff: waiting less just earns another refusal.
        let asked = retry_after.map(|d| d.min(RETRY_AFTER_MAX));
        let hosts = self.active_attempt_hosts();
        let active_on_host = u32::try_from(
            self.attempts
                .iter()
                .filter(|(id, attempt)| {
                    link.is_none_or(|link| attempt.link == link)
                        && hosts.get(*id).is_some_and(|h| h.as_str() == host)
                })
                .count(),
        )
        .unwrap_or(u32::MAX);
        let state = self.hosts.entry((host.to_string(), link)).or_default();
        // Refusals that arrive while the host is already cooling down are
        // one burst (a batch of simultaneous requests all turned away
        // together), not separate offences: only the first escalates the
        // backoff. Every refusal still tightens the learned cap, to the
        // number of connections that were accepted alongside it.
        let halved = (active_on_host / 2).max(1);
        state.learned_cap = Some(state.learned_cap.map_or(halved, |cap| cap.min(halved)));
        if let Some(until) = state.cooldown_until.filter(|until| now < *until) {
            // Same burst — but honor a longer wait if this refusal asks.
            let until = asked.map_or(until, |asked| until.max(now + asked));
            if Some(until) != state.cooldown_until {
                state.cooldown_until = Some(until);
                effects.push(Effect::WakeAt(until));
            }
            return until;
        }
        state.strikes += 1;
        let backoff = HOST_BACKOFF_BASE
            .saturating_mul(1u32 << (state.strikes - 1).min(16))
            .min(HOST_BACKOFF_MAX);
        let backoff = asked.map_or(backoff, |asked| backoff.max(asked));
        let until = now + backoff;
        tracing::debug!(
            host,
            ?link,
            strikes = state.strikes,
            ?backoff,
            cap = ?state.learned_cap,
            "host refused; backing off"
        );
        state.cooldown_until = Some(until);
        effects.push(Effect::WakeAt(until));
        until
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
        // A failed file's other blocks have nothing left to do.
        let doomed: Vec<AttemptId> = self
            .attempts
            .iter()
            .filter(|(_, attempt)| attempt.file == file_id)
            .map(|(id, _)| *id)
            .collect();
        for id in doomed {
            if let Some(attempt) = self.attempts.remove(&id) {
                if let Some(block) = self.blocks.get_mut(&(attempt.file, attempt.block)) {
                    block.in_flight.retain(|a| *a != id);
                }
                effects.push(Effect::AbortAttempt {
                    id,
                    reason: AbortReason::Cancel,
                });
            }
        }
        effects.push(Effect::Emit(EngineEvent::FileFailed {
            file: file_id,
            error,
        }));
    }

    /// `RestartFile`: throws a failed file's progress away and starts it
    /// over from a fresh probe (§10: "the user can restart the file").
    fn restart_file(&mut self, file_id: FileId, now: Instant, effects: &mut Vec<Effect>) {
        let Some(&(package_id, index)) = self.file_index.get(&file_id) else {
            return;
        };
        let Some(package) = self.package_mut(package_id) else {
            return;
        };
        if !matches!(package.files[index].status, FileStatus::Failed(_)) {
            return;
        }
        let dest_dir = package.dest_dir.clone();
        let file = &mut package.files[index];
        let name = file.name.clone();
        file.status = FileStatus::Unprobed;
        file.url = None;
        file.expected_size = None;
        file.supports_ranges = false;
        file.versions.clear();
        file.layout = None;
        file.parts.clear();
        // A package that failed only because of its failed files is
        // live again; one the user cancelled stays cancelled.
        if matches!(
            package.status,
            PackageStatus::Failed(PackageError::FilesFailed | PackageError::VerifyFailed { .. })
        ) {
            package.status = PackageStatus::Downloading;
        }
        self.blocks.retain(|(f, _), _| *f != file_id);
        self.reresolved.remove(&file_id);
        self.refreshing.remove(&file_id);
        self.probe_waits.remove(&file_id);
        self.probing.remove(&file_id);
        effects.push(Effect::DiscardFile {
            file: file_id,
            dest_dir,
            name,
        });
        self.checkpoint_now(package_id, now, effects);
        effects.push(Effect::Emit(EngineEvent::FileRestarted { file: file_id }));
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
            let Some(attempt) = self.attempts.get_mut(&attempt_id) else {
                continue;
            };
            if position > attempt.position {
                attempt.last_advance = now;
            }
            attempt.position = attempt.position.max(position);
            attempt.sample(now, self.config.refresh_slow_for);
            let attempt = attempt.clone();
            let Some(&(package_id, index)) = self.file_index.get(&attempt.file) else {
                continue;
            };
            self.apply_position(package_id, index, attempt.block, attempt.link, position);
            touched.insert(package_id);
        }
        self.update_meters(now);
        self.refresh_slow_connections(now, effects);
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

    /// Slow-connection refresh (§7.5): aborts, with reason `Refresh`, a
    /// connection that has gone silent while others progress, or one that
    /// has run below `refresh_slow_ratio` of **its own link's** fair share
    /// (the link's throughput over its connections) for `refresh_slow_for`
    /// (after `refresh_warmup`). Links are never compared with each other. The
    /// block resumes from its frontier on a new connection; no retry is
    /// counted, and a block is refreshed at most `max_refreshes_per_block`
    /// times.
    fn refresh_slow_connections(&mut self, now: Instant, effects: &mut Vec<Effect>) {
        let config = &self.config;
        // "Others are progressing": some attempt advanced very recently.
        let recent = config.tick_interval * 4 + Duration::from_secs(1);
        let span = config.refresh_slow_for;
        let ids: Vec<AttemptId> = self.attempts.keys().copied().collect();
        let mut to_refresh = Vec::new();

        for id in &ids {
            let attempt = &self.attempts[id];
            let key = (attempt.file, attempt.block);
            let block = self.blocks.get(&key);
            // A raced block is the hedge's business; a spent budget stays spent.
            if block.is_some_and(|b| {
                b.in_flight.len() != 1 || b.refreshes >= config.max_refreshes_per_block
            }) {
                continue;
            }
            let age = now.saturating_duration_since(attempt.started_at);

            let others_progress = self.attempts.iter().any(|(other_id, other)| {
                other_id != id && now.saturating_duration_since(other.last_advance) <= recent
            });
            let silent_for = now.saturating_duration_since(attempt.last_advance);
            if silent_for >= config.refresh_silent_after && others_progress {
                to_refresh.push(*id);
                continue;
            }

            // Crawling relative to this link's own connections, judged
            // over the last `refresh_slow_for` once the warm-up is past.
            let limited = self
                .package_ref(attempt.package)
                .is_some_and(|p| p.limit.is_some());
            if limited || age < config.refresh_warmup + span {
                continue;
            }
            let Some(mine) = attempt.window_speed(now, span) else {
                continue;
            };
            // The link's fair share per connection: its smoothed total
            // throughput over the connections running on it. (Peers come
            // and go every few seconds, so comparing with each peer's
            // own window would rarely have a full window to compare.)
            let on_link = self
                .attempts
                .values()
                .filter(|other| other.link == attempt.link)
                .count();
            if on_link < 2 {
                continue;
            }
            let link_speed = self.meters.get(&attempt.link).map_or(0.0, |m| m.speed);
            let share = link_speed / on_link as f64;
            if share > 0.0 && mine < config.refresh_slow_ratio * share {
                to_refresh.push(*id);
            }
        }

        for id in to_refresh {
            if let Some(attempt) = self.attempts.remove(&id) {
                let block = self
                    .blocks
                    .entry((attempt.file, attempt.block))
                    .or_default();
                block.in_flight.retain(|a| *a != id);
                block.refreshes += 1;
                effects.push(Effect::AbortAttempt {
                    id,
                    reason: AbortReason::Refresh,
                });
            }
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
        // Only a dirty package is rewritten: one whose state still matches
        // its last manifest costs nothing, however many are queued.
        if self.write_checkpoint(package_id, false, effects) {
            self.mark_checkpointed(package_id, now);
        } else if let Some(checkpoint) = self.checkpoints.get_mut(&package_id) {
            checkpoint.last = Some(now);
        }
    }

    /// Queues a `Checkpoint` for `package_id` and remembers what it
    /// holds. Unless `force`, does nothing (returns false) when the
    /// package is unchanged since its last one.
    fn write_checkpoint(
        &mut self,
        package_id: PackageId,
        force: bool,
        effects: &mut Vec<Effect>,
    ) -> bool {
        let Some(package) = self.package_ref(package_id) else {
            return false;
        };
        if !force && self.written.get(&package_id) == Some(package) {
            return false;
        }
        let package = package.clone();
        effects.push(Effect::Checkpoint {
            package: package_id,
            manifest: PackageManifest::new(package.clone()),
        });
        self.written.insert(package_id, package);
        true
    }

    fn mark_checkpointed(&mut self, package_id: PackageId, now: Instant) {
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
        let racing = {
            let block = self.blocks.entry(block_key).or_default();
            block.in_flight.retain(|a| *a != id);
            if delivered == 0 {
                block.avoid_links.insert(attempt.link);
            } else {
                block.avoid_links.remove(&attempt.link);
            }
            !block.in_flight.is_empty()
        };
        // A raced block (§7.5) that is now complete: whoever finished it
        // won, and the rest are aborted as `Lost`.
        if racing && self.block_complete(package_id, index, attempt.block) {
            self.abort_block_racers(block_key, effects);
        }
        // While another attempt still runs the block, a failure here
        // costs the block nothing: no retry is counted and no backoff
        // holds it up.
        let racing = racing
            && self
                .blocks
                .get(&block_key)
                .is_some_and(|b| !b.in_flight.is_empty());
        if delivered > 0 {
            self.host_delivered(package_id, index, attempt.link);
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
            AttemptOutcome::Failed(error)
                if matches!(error, crate::error::TransferError::Status(403 | 410))
                    && self.begin_reresolve(package_id, index, attempt.file, effects) =>
            {
                // Absorbed: the URL probably expired; the block waits (no
                // retry counted) while the source is resolved again.
            }
            AttemptOutcome::Failed(error) => {
                if blame(&error) == Blame::Host {
                    let retry_after = match &error {
                        crate::error::TransferError::Busy { retry_after, .. } => *retry_after,
                        _ => None,
                    };
                    self.host_refused(package_id, index, attempt.link, retry_after, now, effects);
                }
                if blame(&error) == Blame::Link {
                    let health = self.health.entry(attempt.link).or_default();
                    if let Some(until) = health.record_failure(now, &self.config) {
                        effects.push(Effect::WakeAt(until));
                    }
                }
                let file_wide = matches!(blame(&error), Blame::File | Blame::Fatal);
                if !racing || file_wide {
                    self.handle_attempt_failure(package_id, index, block_key, &error, now, effects);
                }
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

    /// Whether every byte of the block is durable.
    fn block_complete(&self, package_id: PackageId, index: usize, block: u32) -> bool {
        let Some(file) = self.package_ref(package_id).map(|p| &p.files[index]) else {
            return false;
        };
        let Some(layout) = file.layout else {
            return false;
        };
        let total = file.expected_size.unwrap_or(0);
        plan::block_len(&layout, total, block).is_some_and(|len| {
            file.parts
                .get(block as usize)
                .is_some_and(|part| part.durable >= len)
        })
    }

    /// Aborts every attempt still running `block_key` — the losers of a
    /// hedge race.
    fn abort_block_racers(&mut self, block_key: (FileId, u32), effects: &mut Vec<Effect>) {
        let losers = self
            .blocks
            .get_mut(&block_key)
            .map(|block| std::mem::take(&mut block.in_flight))
            .unwrap_or_default();
        for id in losers {
            if self.attempts.remove(&id).is_some() {
                effects.push(Effect::AbortAttempt {
                    id,
                    reason: AbortReason::Lost,
                });
            }
        }
    }

    /// An immediate checkpoint, outside the tick cadence (§9.3: "on pause,
    /// file completion, and shutdown").
    fn checkpoint_now(&mut self, package_id: PackageId, now: Instant, effects: &mut Vec<Effect>) {
        if self.write_checkpoint(package_id, true, effects) {
            self.mark_checkpointed(package_id, now);
        }
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
            // §16: a busy host is not the block's fault — back the host
            // off and learn a lower connection cap; the block itself is
            // not charged a retry. A host that never relents fails the
            // file rather than looping forever.
            // Handled in `handle_attempt_ended`, which knows the link.
            Blame::Host => {}
            Blame::Attempt => {
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
                let failure = match error {
                    // §10: fails this file only; the user can restart it.
                    crate::error::TransferError::RemoteChanged(detail) => {
                        FileError::RemoteChanged {
                            detail: detail.clone(),
                        }
                    }
                    other => FileError::Other {
                        detail: other.to_string(),
                    },
                };
                self.fail_file(package_id, index, failure, effects);
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
            PackageStatus::Done
                | PackageStatus::Failed(_)
                | PackageStatus::Verifying
                | PackageStatus::Extracting
        ) {
            return;
        }
        if package.all_files_verified() {
            self.begin_post(package_id, effects);
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

    // --- post-processing: verify, extract (§12.3, §12.4) ---

    /// Every file is on disk: verify the package.
    fn begin_post(&mut self, package_id: PackageId, effects: &mut Vec<Effect>) {
        let Some(package) = self.package_mut(package_id) else {
            return;
        };
        package.status = PackageStatus::Verifying;
        let dest_dir = package.dest_dir.clone();
        let files = package
            .files
            .iter()
            .map(|file| crate::verify::VerifyFile {
                name: file.name.clone(),
                expected_size: file.expected_size,
            })
            .collect();
        self.write_checkpoint(package_id, true, effects);
        effects.push(Effect::Emit(EngineEvent::PackageVerifying {
            package: package_id,
        }));
        effects.push(Effect::VerifyPackage {
            package: package_id,
            dest_dir,
            files,
        });
    }

    fn handle_verify_done(
        &mut self,
        package_id: PackageId,
        result: Result<crate::verify::VerifyReport, crate::verify::VerifyFailure>,
        effects: &mut Vec<Effect>,
    ) {
        let Some(package) = self.package_ref(package_id) else {
            return;
        };
        if package.status != PackageStatus::Verifying {
            return;
        }
        match result {
            Ok(_) => {
                let wants_extract = package.extract && package.archive.is_some();
                if wants_extract {
                    self.extract_queue.push_back(package_id);
                    self.pump_extract(effects);
                } else {
                    self.finish_package(package_id, effects);
                }
            }
            Err(failure) => {
                let dest_dir = package.dest_dir.clone();
                let bad_file = failure
                    .file
                    .as_ref()
                    .and_then(|name| package.files.iter().position(|file| &file.name == name));
                let detail = failure.detail.clone();
                // A corrupt file is deleted and marked failed, so the
                // user's "restart file" fetches it afresh.
                if failure.corrupt
                    && let Some(index) = bad_file
                {
                    let name = package.files[index].name.clone();
                    self.fail_file(
                        package_id,
                        index,
                        FileError::VerifyFailed {
                            detail: detail.clone(),
                        },
                        effects,
                    );
                    effects.push(Effect::DeleteFiles {
                        dest_dir,
                        names: vec![name],
                    });
                }
                self.fail_package(package_id, PackageError::VerifyFailed { detail }, effects);
            }
        }
    }

    /// Starts the next waiting extraction if the pool (one) is free.
    fn pump_extract(&mut self, effects: &mut Vec<Effect>) {
        while self.extracting.is_none() {
            let Some(package_id) = self.extract_queue.pop_front() else {
                return;
            };
            let Some(package) = self.package_ref(package_id) else {
                continue;
            };
            if package.status != PackageStatus::Verifying {
                continue;
            }
            let Some(spec) = self.extract_spec(package) else {
                self.fail_package(
                    package_id,
                    PackageError::ExtractFailed {
                        detail: "couldn't tell which files make up the archive".to_string(),
                    },
                    effects,
                );
                continue;
            };
            if let Some(package) = self.package_mut(package_id) {
                package.status = PackageStatus::Extracting;
            }
            self.extracting = Some(package_id);
            self.write_checkpoint(package_id, true, effects);
            effects.push(Effect::Emit(EngineEvent::PackageExtracting {
                package: package_id,
            }));
            effects.push(Effect::Extract {
                package: package_id,
                spec,
            });
        }
    }

    /// The archive's volumes, in reading order, among the package's
    /// files (a checksum sidecar in the same package is not one).
    fn extract_spec(&self, package: &Package) -> Option<crate::extract::ExtractSpec> {
        let archive = package.archive.as_ref()?;
        let names: Vec<&str> = package.files.iter().map(|f| f.name.as_str()).collect();
        let group = crate::group::group_names(&names)
            .into_iter()
            .find(|g| g.archive.as_ref() == Some(archive))?;
        Some(crate::extract::ExtractSpec {
            scheme: archive.scheme,
            base_name: archive.base_name.clone(),
            volumes: group
                .members
                .iter()
                .map(|&i| package.dest_dir.join(&package.files[i].name))
                .collect(),
            dest_dir: package.dest_dir.clone(),
            limits: crate::extract::ExtractLimits::from_config(&self.config),
        })
    }

    fn handle_extract_done(
        &mut self,
        package_id: PackageId,
        result: Result<crate::extract::ExtractSummary, String>,
        effects: &mut Vec<Effect>,
    ) {
        if self.extracting == Some(package_id) {
            self.extracting = None;
        }
        let still_extracting = self
            .package_ref(package_id)
            .is_some_and(|p| p.status == PackageStatus::Extracting);
        if still_extracting {
            match result {
                Ok(_) => {
                    // Archives are deleted only after extraction
                    // succeeded, and only the ones that were extracted.
                    if let Some(package) = self.package_ref(package_id)
                        && package.delete_archives
                        && let Some(spec) = self.extract_spec(package)
                    {
                        let names = spec
                            .volumes
                            .iter()
                            .filter_map(|v| v.file_name())
                            .map(|n| n.to_string_lossy().into_owned())
                            .collect();
                        effects.push(Effect::DeleteFiles {
                            dest_dir: spec.dest_dir,
                            names,
                        });
                    }
                    self.finish_package(package_id, effects);
                }
                Err(detail) => {
                    self.fail_package(package_id, PackageError::ExtractFailed { detail }, effects)
                }
            }
        }
        self.pump_extract(effects);
    }

    fn finish_package(&mut self, package_id: PackageId, effects: &mut Vec<Effect>) {
        if let Some(package) = self.package_mut(package_id) {
            package.status = PackageStatus::Done;
        }
        self.write_checkpoint(package_id, true, effects);
        effects.push(Effect::Emit(EngineEvent::PackageDone {
            package: package_id,
        }));
    }

    fn fail_package(
        &mut self,
        package_id: PackageId,
        error: PackageError,
        effects: &mut Vec<Effect>,
    ) {
        if let Some(package) = self.package_mut(package_id) {
            package.status = PackageStatus::Failed(error.clone());
        }
        self.write_checkpoint(package_id, true, effects);
        effects.push(Effect::Emit(EngineEvent::PackageFailed {
            package: package_id,
            error,
        }));
    }

    /// A cancelled or removed package leaves the post-processing queue,
    /// and anything it has running is stopped.
    fn stop_post(&mut self, package_id: PackageId, effects: &mut Vec<Effect>) {
        self.extract_queue.retain(|id| *id != package_id);
        let running = self.package_ref(package_id).is_some_and(|p| {
            matches!(
                p.status,
                PackageStatus::Verifying | PackageStatus::Extracting
            )
        });
        if running {
            effects.push(Effect::CancelPost {
                package: package_id,
            });
        }
        if self.extracting == Some(package_id) {
            self.extracting = None;
            self.pump_extract(effects);
        }
    }

    fn set_package_limit(
        &mut self,
        package_id: PackageId,
        limit: Option<crate::model::ByteRate>,
        now: Instant,
        effects: &mut Vec<Effect>,
    ) {
        let Some(package) = self.package_mut(package_id) else {
            return;
        };
        if package.limit == limit {
            return;
        }
        package.limit = limit;
        effects.push(Effect::SetPackageLimit {
            package: package_id,
            limit,
        });
        self.checkpoint_now(package_id, now, effects);
        effects.push(Effect::Emit(EngineEvent::PackageLimitChanged {
            package: package_id,
        }));
    }

    fn set_post_options(
        &mut self,
        package_id: PackageId,
        extract: bool,
        delete_archives: bool,
        now: Instant,
        effects: &mut Vec<Effect>,
    ) {
        let Some(package) = self.package_mut(package_id) else {
            return;
        };
        if package.extract == extract && package.delete_archives == delete_archives {
            return;
        }
        package.extract = extract;
        package.delete_archives = delete_archives;
        self.checkpoint_now(package_id, now, effects);
    }

    // --- scheduling (§7.2) ---

    /// Every block that could start now on `link`.
    fn build_candidates(&self, now: Instant, link: LinkId) -> Vec<sched::FileCandidate> {
        let mut out = Vec::new();
        let mut active_by_host: HashMap<&str, u32> = HashMap::new();
        for attempt in self.attempts.values().filter(|a| a.link == link) {
            if let Some(host) = self
                .file_index
                .get(&attempt.file)
                .and_then(|&(p, i)| self.package_ref(p)?.files[i].url.as_ref())
                .and_then(|url| url.host_str())
            {
                *active_by_host.entry(host).or_default() += 1;
            }
        }
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
                if self.refreshing.contains(&file.id) {
                    continue;
                }
                let Some(host) = url.host_str() else { continue };
                if let Some(state) = self.hosts.get(&(host.to_string(), Some(link))) {
                    if state.cooldown_until.is_some_and(|until| now < until) {
                        continue;
                    }
                    if state
                        .learned_cap
                        .is_some_and(|cap| active_by_host.get(host).copied().unwrap_or(0) >= cap)
                    {
                        continue;
                    }
                }
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
                    if runtime.is_some_and(|block| !block.in_flight.is_empty()) {
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

    /// Whether a usable link other than `link` has a free slot.
    fn other_link_has_room(&self, link: LinkId, now: Instant) -> bool {
        self.links.iter().filter(|l| l.id != link).any(|l| {
            let cap = self
                .health
                .get(&l.id)
                .map_or(u32::from(l.max_slots), |health| {
                    health.slot_cap(u32::from(l.max_slots), now)
                });
            let active = self.attempts.values().filter(|a| a.link == l.id).count();
            u32::try_from(active).unwrap_or(u32::MAX) < cap
        })
    }

    /// Whether any work short of hedging remains (§7.2 pass 3's
    /// precondition): a file still to probe or activate, or a block of an
    /// active file that nothing is running — including one waiting out a
    /// retry backoff or a host cooldown, which will be real work soon.
    fn has_pending_work(&self) -> bool {
        for package in &self.packages {
            if package.status != PackageStatus::Downloading {
                continue;
            }
            for file in &package.files {
                match file.status {
                    FileStatus::Unprobed | FileStatus::Queued | FileStatus::Preparing => {
                        return true;
                    }
                    FileStatus::Active => {}
                    _ => continue,
                }
                if self.refreshing.contains(&file.id) {
                    return true;
                }
                let Some(layout) = file.layout else { continue };
                let total = file.expected_size.unwrap_or(0);
                for index in 0..layout.count {
                    let done = plan::block_len(&layout, total, index).is_some_and(|len| {
                        file.parts
                            .get(index as usize)
                            .is_some_and(|part| part.durable >= len)
                    });
                    let running = self
                        .blocks
                        .get(&(file.id, index))
                        .is_some_and(|block| !block.in_flight.is_empty());
                    if !done && !running {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Blocks pass 3 could race on `link`: held by exactly one attempt,
    /// on a file whose server supports ranges, with a bounded remainder.
    fn hedge_candidates(&self, now: Instant, link: LinkId) -> Vec<sched::HedgeCandidate> {
        let mut out = Vec::new();
        for ((file_id, block_index), runtime) in &self.blocks {
            let [holder_id] = runtime.in_flight.as_slice() else {
                continue;
            };
            let Some(holder) = self.attempts.get(holder_id) else {
                continue;
            };
            let Some(&(package_id, index)) = self.file_index.get(file_id) else {
                continue;
            };
            let Some(package) = self.package_ref(package_id) else {
                continue;
            };
            if package.status != PackageStatus::Downloading {
                continue;
            }
            let file = &package.files[index];
            if file.status != FileStatus::Active || !file.supports_ranges {
                continue;
            }
            let (Some(layout), Some(host)) =
                (file.layout, file.url.as_ref().and_then(|u| u.host_str()))
            else {
                continue;
            };
            if self
                .hosts
                .get(&(host.to_string(), Some(link)))
                .and_then(|state| state.cooldown_until)
                .is_some_and(|until| now < until)
            {
                continue;
            }
            let total = file.expected_size.unwrap_or(0);
            let Some(block_range) = plan::block_range(&layout, total, *block_index) else {
                continue;
            };
            let Some(end) = block_range.end else { continue };
            let durable = file
                .parts
                .get(*block_index as usize)
                .map_or(0, |part| part.durable);
            let Some(range) = crate::model::ByteRange::bounded(block_range.start + durable, end)
            else {
                continue;
            };
            let age = now.saturating_duration_since(holder.started_at);
            let delivered = holder.position.saturating_sub(holder.range_start);
            let eta = (delivered > 0 && !age.is_zero()).then(|| {
                let speed = delivered as f64 / age.as_secs_f64();
                let left = (end + 1).saturating_sub(holder.position) as f64;
                Duration::from_secs_f64((left / speed).min(1e9))
            });
            out.push(sched::HedgeCandidate {
                file: *file_id,
                block: *block_index,
                host: host.to_string(),
                range,
                holder_link: holder.link,
                holder_age: age,
                eta,
                hedges: runtime.hedges,
            });
        }
        out
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

    /// Files whose placeholder is being created right now.
    fn preparing_count(&self) -> u32 {
        u32::try_from(
            self.packages
                .iter()
                .flat_map(|package| package.files.iter())
                .filter(|file| file.status == FileStatus::Preparing)
                .count(),
        )
        .unwrap_or(u32::MAX)
    }

    /// Slots that could take work right now, across every link.
    fn free_slot_count(&self, now: Instant) -> u32 {
        let mut free = 0u32;
        for link in &self.links {
            let cap = self
                .health
                .get(&link.id)
                .map_or(u32::from(link.max_slots), |health| {
                    health.slot_cap(u32::from(link.max_slots), now)
                });
            let active = u32::try_from(
                self.attempts
                    .values()
                    .filter(|attempt| attempt.link == link.id)
                    .count(),
            )
            .unwrap_or(u32::MAX);
            free = free.saturating_add(cap.saturating_sub(active));
        }
        let room = self
            .config
            .max_connections_total
            .saturating_sub(u32::try_from(self.attempts.len()).unwrap_or(u32::MAX));
        free.min(room)
    }

    /// Lazy activation (§7.3): starts preparing the highest-ranked
    /// `Queued` file, if free slots exist that files already preparing
    /// won't fill and the file's host allows another active file. The
    /// `PrepareFile` effect creates and preallocates its placeholder;
    /// the file turns `Active` — and gets attempts — when the driver
    /// reports it prepared. Returns whether one was activated.
    fn try_activate_one(&mut self, now: Instant, effects: &mut Vec<Effect>) -> bool {
        if self.preparing_count() >= self.free_slot_count(now) {
            return false;
        }

        // Queue order: priority, then position (§4).
        let mut order: Vec<usize> = (0..self.packages.len()).collect();
        order.sort_by(|&a, &b| {
            self.packages[b]
                .priority
                .cmp(&self.packages[a].priority)
                .then(a.cmp(&b))
        });
        for package_index in order {
            let package = &self.packages[package_index];
            if package.status != PackageStatus::Downloading {
                continue;
            }
            for (file_index, file) in package.files.iter().enumerate() {
                if file.status != FileStatus::Queued {
                    continue;
                }
                if let Some(host) = file.url.as_ref().and_then(|url| url.host_str())
                    && let Some(max) = self.config.hosts.limits_for(host).max_active_files
                {
                    let active_for_host = self
                        .packages
                        .iter()
                        .flat_map(|p| p.files.iter())
                        .filter(|f| {
                            matches!(f.status, FileStatus::Active | FileStatus::Preparing)
                                && f.url.as_ref().and_then(|u| u.host_str()) == Some(host)
                        })
                        .count();
                    if u32::try_from(active_for_host).unwrap_or(u32::MAX) >= max {
                        continue;
                    }
                }
                let plan = FilePrep {
                    package: package.id,
                    dest_dir: package.dest_dir.clone(),
                    suggested_name: file.name.clone(),
                    expected_size: file.expected_size,
                    resume_final_name: None,
                };
                let file_id = file.id;
                let package_id = package.id;
                if let Some(package) = self.package_mut(package_id) {
                    package.files[file_index].status = FileStatus::Preparing;
                }
                effects.push(Effect::PrepareFile {
                    file: file_id,
                    plan,
                });
                return true;
            }
        }
        false
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

        let candidates = self.build_candidates(now, link.id);
        // §7.2: spread first; then activate a queued file if one could use
        // this slot; only then focus (a lone big file taking every slot).
        // Both picks are made up front so the borrows of `self` end before
        // activation mutates it.
        let (spread, focus) = {
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
            let spread = sched::pick_spread(&view, link.id);
            let focus = if spread.is_none() {
                sched::pick_focus(&view, link.id)
            } else {
                None
            };
            // Pass 3 only when nothing at all is left to do (§7.2, §7.5).
            let hedge = if focus.is_none() && !self.has_pending_work() {
                let mut hedges = self.hedge_candidates(now, link.id);
                // Race on another path when one has room (§7.5): a block
                // held on this link waits for that link's free slot.
                hedges.retain(|c| {
                    c.holder_link != link.id || !self.other_link_has_room(link.id, now)
                });
                sched::pick_hedge(&hedges, &active, &self.config.hosts, &self.config, link.id)
            } else {
                None
            };
            (spread, focus.or(hedge))
        };
        let work = match spread {
            Some(work) => work,
            None => {
                if self.try_activate_one(now, effects) {
                    return true;
                }
                // Slots that files still preparing will need are kept
                // free for them, instead of being soaked up by a big
                // file's focus pass just before they become Active.
                if self.preparing_count() > 0 && self.free_slot_count(now) <= self.preparing_count()
                {
                    return false;
                }
                match focus {
                    Some(work) => work,
                    None => return false,
                }
            }
        };
        let (file, block, range, file_offset, is_hedge) = match work {
            sched::Work::Primary {
                file,
                block,
                range,
                file_offset,
            } => (file, block, range, file_offset, false),
            sched::Work::Hedge {
                file,
                block,
                range,
                file_offset,
            } => (file, block, range, file_offset, true),
        };
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
                started_at: now,
                position: range.start,
                last_advance: now,
                samples: std::collections::VecDeque::from([(now, range.start)]),
            },
        );
        let runtime = self.blocks.entry((file, block)).or_default();
        runtime.in_flight.push(id);
        if is_hedge {
            runtime.hedges += 1;
        }
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

/// Applies a command's post-processing options to a new package.
fn apply_post(package: &mut Package, post: PostOptions) {
    package.archive = post.archive;
    package.extract = post.extract && package.archive.is_some();
    package.delete_archives = post.delete_archives;
}

/// What `AddPackage` / `AddProbed` share about the package they create.
struct NewPackage {
    name: String,
    dest_dir: std::path::PathBuf,
    priority: crate::model::Priority,
    post: PostOptions,
}
