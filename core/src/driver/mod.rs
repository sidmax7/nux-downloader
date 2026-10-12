//! The imperative shell (`ARCHITECTURE.md` §14, §8, §9.3): a single
//! tokio task that owns `EngineState`, executes the effects it returns,
//! and feeds results back in as `Input`s. This is the only place in the
//! crate that touches a socket or a file.
//!
//! Effects fall into two groups:
//! - **Async I/O** (`Probe`, `StartAttempt`) runs on its own spawned
//!   task and reports back through the shared input channel, since a
//!   probe or a transfer can take seconds and must never block the loop.
//! - **`PrepareFile`** (create and preallocate) runs on `spawn_blocking`
//!   but is *awaited inline*: it is cheap local work and its result
//!   feeds straight back into the engine.
//! - **Durability work** (`Checkpoint`, `Finalize`) and the events that
//!   announce it go through one ordered *lane* — a task that runs them
//!   strictly in the order they were issued. An `fsync` can take
//!   milliseconds, and awaiting it inline stalled every slot behind
//!   every finished file; on the lane it stalls nothing, yet an event
//!   still never overtakes the checkpoint it announces (§9.3).

pub mod prepare;
pub mod transfer;

use crate::config::EngineConfig;
use crate::engine::{Command, Effect, EngineEvent, EngineSnapshot, EngineState, Input};
use crate::error::{ProbeError, StorageError};
use crate::model::{AttemptId, FileId, Link, LinkId};
use crate::net::bind::{BindPolicy, build_bound_client};
use crate::net::client::{build_default_client, build_link_client};
use crate::net::monitor::{self, LinkSetup, MonitorSetup};
use prepare::OpenFile;
use reqwest::Client;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::warn;

/// Work for the ordered durability lane (see the module docs).
enum LaneJob {
    Checkpoint {
        package: crate::model::PackageId,
        files: Vec<(Arc<std::fs::File>, PathBuf)>,
        manifest: crate::storage::manifest::PackageManifest,
    },
    Finalize {
        file: FileId,
        open: OpenFile,
    },
    Emit(EngineEvent),
    /// Replies once everything queued before it has run. A snapshot goes
    /// through one, so it never shows a file as done while its rename is
    /// still waiting on the lane.
    Barrier(tokio::sync::oneshot::Sender<()>),
}

/// Runs lane jobs one after another until every sender is gone.
async fn run_lane(
    mut jobs: mpsc::UnboundedReceiver<LaneJob>,
    manifest_dir: PathBuf,
    input_tx: mpsc::WeakSender<Input>,
    events_tx: broadcast::Sender<EngineEvent>,
) {
    while let Some(job) = jobs.recv().await {
        match job {
            LaneJob::Checkpoint {
                package,
                files,
                manifest,
            } => {
                let dir = manifest_dir.clone();
                let result = tokio::task::spawn_blocking(move || {
                    prepare::checkpoint(&files, &dir, &manifest)
                })
                .await
                .unwrap_or_else(|error| {
                    Err(StorageError::Io {
                        operation: "checkpoint (task panicked)",
                        path: String::new(),
                        source: std::io::Error::other(error.to_string()),
                    })
                });
                if let Some(tx) = input_tx.upgrade() {
                    let _ = tx.send(Input::CheckpointDone { package, result }).await;
                }
            }
            LaneJob::Finalize { file, open } => {
                // `open` moves into the task and is dropped there, so the
                // file is closed by the time `FinalizeDone` is sent.
                match tokio::task::spawn_blocking(move || prepare::finish(&open)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => warn!("driver: finalize failed for {file:?}: {error}"),
                    Err(error) => warn!("driver: finalize task panicked for {file:?}: {error}"),
                }
                if let Some(tx) = input_tx.upgrade() {
                    let _ = tx.send(Input::FinalizeDone { file }).await;
                }
            }
            LaneJob::Emit(event) => {
                let _ = events_tx.send(event);
            }
            LaneJob::Barrier(reached) => {
                let _ = reached.send(());
            }
        }
    }
}

/// A running attempt's shared, driver-owned handles: the engine only
/// keeps ids, but the tick sampler and `AbortAttempt` both need to reach
/// the task itself.
struct RunningAttempt {
    position: Arc<AtomicU64>,
    cancel: CancellationToken,
}

/// A link's client, remembered with the addresses it was built for so a
/// rescan that changes them rebuilds it.
struct LinkClient {
    v4: Option<std::net::Ipv4Addr>,
    v6: Option<std::net::Ipv6Addr>,
    client: Client,
}

/// A read-only query the worker answers directly from its own
/// `EngineState`, without going through `Input`/`Effect` — a query
/// changes nothing, so it doesn't belong in the pure `handle()` pipeline.
enum WorkerRequest {
    Snapshot(oneshot::Sender<EngineSnapshot>),
}

/// A handle to a running driver task. Cloning it is cheap and shares the
/// same engine; `Engine` (P2.8) wraps this in a friendlier API.
#[derive(Clone)]
pub struct Driver {
    limits: Arc<crate::limit::Limits>,
    input_tx: mpsc::Sender<Input>,
    request_tx: mpsc::Sender<WorkerRequest>,
    events_tx: broadcast::Sender<EngineEvent>,
}

impl Driver {
    /// Spawns the driver task and returns a handle to it, starting fresh
    /// with no packages. `links` stands in for `net::discover`'s output
    /// until Phase 3 — M1 callers pass a single synthetic link
    /// (`engine::sim::one_link`-shaped).
    #[must_use]
    pub fn spawn_fresh(config: EngineConfig, links: Vec<Link>, manifest_dir: PathBuf) -> Self {
        Self::spawn(EngineState::new(config), Vec::new(), links, manifest_dir)
    }

    /// Spawns the driver task from an already-built `EngineState` —
    /// fresh (`EngineState::new`) or restored from disk
    /// (`EngineState::restore`, whose own returned effects are
    /// `initial_effects` here). `Engine::start` (P2.8) is the one caller
    /// that needs this restored path; everything else wants
    /// `spawn_fresh`.
    #[must_use]
    pub fn spawn(
        state: EngineState,
        initial_effects: Vec<Effect>,
        links: Vec<Link>,
        manifest_dir: PathBuf,
    ) -> Self {
        Self::spawn_with(
            state,
            initial_effects,
            LinkSetup::Fixed {
                links,
                bind: BindPolicy::Off,
            },
            manifest_dir,
        )
    }

    /// Like `spawn`, but the links come from a `LinkSetup`: a fixed list,
    /// or a monitored, interface-bound set that follows the machine's
    /// network changes (§6.1).
    #[must_use]
    pub fn spawn_with(
        state: EngineState,
        initial_effects: Vec<Effect>,
        links: LinkSetup,
        manifest_dir: PathBuf,
    ) -> Self {
        let (input_tx, input_rx) = mpsc::channel(256);
        let (request_tx, request_rx) = mpsc::channel(32);
        let (events_tx, _) = broadcast::channel(1024);

        let bind = match &links {
            LinkSetup::Fixed { bind, .. } => *bind,
            LinkSetup::Monitored(MonitorSetup { bind, .. }) => *bind,
        };
        let limits = crate::limit::Limits::new(state.config().global_limit);
        let worker = Worker::new(
            state,
            limits.clone(),
            manifest_dir,
            bind,
            input_tx.clone(),
            events_tx.clone(),
        );
        tokio::spawn(worker.run(input_rx, request_rx, links, initial_effects));

        Self {
            limits,
            input_tx,
            request_tx,
            events_tx,
        }
    }

    /// The speed limiters (§11), shared with the running engine: changing
    /// one applies to transfers already in flight.
    #[must_use]
    pub fn limits(&self) -> &Arc<crate::limit::Limits> {
        &self.limits
    }

    /// Sends a command into the engine. Errors only if the driver task
    /// has already shut down.
    pub async fn command(
        &self,
        command: Command,
    ) -> Result<(), Box<mpsc::error::SendError<Input>>> {
        self.input_tx
            .send(Input::Command(command))
            .await
            .map_err(Box::new)
    }

    /// A point-in-time copy of every package's current state. `None` only
    /// if the driver task has already shut down.
    pub async fn snapshot(&self) -> Option<EngineSnapshot> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.request_tx
            .send(WorkerRequest::Snapshot(reply_tx))
            .await
            .ok()?;
        reply_rx.await.ok()
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.events_tx.subscribe()
    }

    /// Sends `Shutdown` and waits for the driver task to finish
    /// checkpointing and exit.
    pub async fn shutdown(self) {
        let Self {
            input_tx,
            events_tx,
            request_tx: _,
            limits: _,
        } = self;
        let mut events = events_tx.subscribe();
        // Drop our own sender clone before waiting: the broadcast channel
        // only closes once every sender is gone, and the worker task's
        // clone is the one whose drop should end this wait (unless the
        // caller kept other `Driver` clones alive too, in which case this
        // is best-effort — see `EngineEvent`'s subscribers in general).
        drop(events_tx);
        if input_tx
            .send(Input::Command(Command::Shutdown))
            .await
            .is_err()
        {
            return;
        }
        while events.recv().await.is_ok() {}
    }
}

/// The driver's own state: everything `EngineState` doesn't own because
/// it isn't pure — open files, in-flight attempts, and the clients used
/// to reach the network.
struct Worker {
    state: EngineState,
    limits: Arc<crate::limit::Limits>,
    default_client: Client,
    lane: mpsc::UnboundedSender<LaneJob>,
    lane_task: Option<tokio::task::JoinHandle<()>>,
    registry: crate::resolve::Registry,
    link_clients: HashMap<LinkId, LinkClient>,
    /// How link clients are pinned to their link.
    bind: BindPolicy,
    open_files: HashMap<FileId, OpenFile>,
    running: HashMap<AttemptId, RunningAttempt>,
    input_tx: mpsc::Sender<Input>,
    events_tx: broadcast::Sender<EngineEvent>,
    /// Cancel flags of the verification / extraction running per package.
    post_cancel: HashMap<crate::model::PackageId, Arc<std::sync::atomic::AtomicBool>>,
}

impl Worker {
    fn new(
        state: EngineState,
        limits: Arc<crate::limit::Limits>,
        manifest_dir: PathBuf,
        bind: BindPolicy,
        input_tx: mpsc::Sender<Input>,
        events_tx: broadcast::Sender<EngineEvent>,
    ) -> Self {
        let default_client = build_default_client(state.config()).unwrap_or_else(|error| {
            panic!("driver: couldn't build the default HTTP client: {error}")
        });
        let (lane, lane_jobs) = mpsc::unbounded_channel();
        let lane_task = tokio::spawn(run_lane(
            lane_jobs,
            manifest_dir.clone(),
            input_tx.downgrade(),
            events_tx.clone(),
        ));
        Self {
            state,
            limits,
            default_client,
            lane,
            lane_task: Some(lane_task),
            registry: crate::resolve::Registry::default(),
            link_clients: HashMap::new(),
            bind,
            open_files: HashMap::new(),
            running: HashMap::new(),
            input_tx,
            events_tx,
            post_cancel: HashMap::new(),
        }
    }

    /// Brings the per-link clients in line with a fresh link set: builds
    /// one for each new link (or one whose addresses changed), drops the
    /// rest, and returns only the links that have a working client — a
    /// link that can't be bound is left out rather than fed to the engine
    /// to fail on every attempt.
    fn sync_clients(&mut self, links: Vec<Link>) -> Vec<Link> {
        let config = self.state.config().clone();
        let mut clients = HashMap::new();
        let mut usable = Vec::new();
        for link in links {
            let existing = self
                .link_clients
                .remove(&link.id)
                .filter(|held| held.v4 == link.v4 && held.v6 == link.v6);
            let held = match existing {
                Some(held) => held,
                None => {
                    let built = match self.bind.mode_for(&link) {
                        Some(mode) => build_bound_client(&config, &link, &mode),
                        None if self.bind == BindPolicy::Off => build_link_client(&config),
                        None => Err(crate::error::NetError::NoBindAddress {
                            link: link.name.clone(),
                        }),
                    };
                    match built {
                        Ok(client) => LinkClient {
                            v4: link.v4,
                            v6: link.v6,
                            client,
                        },
                        Err(error) => {
                            warn!("driver: skipping link {}: {error}", link.name);
                            continue;
                        }
                    }
                }
            };
            clients.insert(link.id, held);
            self.limits.ensure_link(link.id, link.limit);
            usable.push(link);
        }
        self.link_clients = clients;
        usable
    }

    async fn run(
        mut self,
        mut input_rx: mpsc::Receiver<Input>,
        mut request_rx: mpsc::Receiver<WorkerRequest>,
        setup: LinkSetup,
        initial_effects: Vec<Effect>,
    ) {
        // `LinksChanged` is folded in here rather than by the caller, so
        // its own effects (e.g. `StartAttempt`s a restored, already-Active
        // file is now ready for) join `initial_effects` in one drain
        // instead of being silently dropped.
        let links = match setup {
            LinkSetup::Fixed { links, .. } => links,
            LinkSetup::Monitored(monitor_setup) => {
                let config = self.state.config().clone();
                let found = monitor::scan(&monitor_setup, &config, &[])
                    .await
                    .unwrap_or_else(|error| {
                        warn!("driver: initial link scan failed: {error}");
                        Vec::new()
                    });
                let found = self.sync_clients(found);
                monitor::spawn_watch(
                    monitor_setup,
                    config,
                    found.clone(),
                    self.input_tx.downgrade(),
                );
                found
            }
        };
        let links = self.sync_clients(links);
        // Limits that outlive a restart: each package's own.
        for package in self.state.packages() {
            self.limits.set_package(package.id, package.limit);
        }
        let now = Instant::now();
        let links_effects = self.state.handle(Input::LinksChanged(links), now);
        self.drain(
            initial_effects.into_iter().chain(links_effects).collect(),
            now,
        )
        .await;

        let tick_interval = self.state.config().tick_interval;
        let mut ticker = tokio::time::interval(tick_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick fires immediately; nothing is running yet.
        ticker.tick().await;

        loop {
            tokio::select! {
                biased;
                input = input_rx.recv() => match input {
                    Some(input) => {
                        let is_shutdown = matches!(input, Input::Command(Command::Shutdown));
                        self.step(input).await;
                        if is_shutdown {
                            break;
                        }
                    }
                    None => break,
                },
                Some(request) = request_rx.recv() => self.handle_request(request),
                _ = ticker.tick() => {
                    let progress = self.sample_positions();
                    self.step(Input::Tick { progress }).await;
                }
            }
        }

        // Let the queued checkpoints, renames and events finish before
        // this task ends: a shutdown's final checkpoint must have landed.
        drop(std::mem::replace(
            &mut self.lane,
            mpsc::unbounded_channel().0,
        ));
        if let Some(task) = self.lane_task.take() {
            let _ = task.await;
        }
    }

    fn handle_request(&self, request: WorkerRequest) {
        match request {
            WorkerRequest::Snapshot(reply) => {
                // Take the state now, but hand it out only once the
                // durability work already issued has landed.
                let snapshot = self.state.snapshot();
                let (reached, wait) = tokio::sync::oneshot::channel();
                if self.lane.send(LaneJob::Barrier(reached)).is_ok() {
                    tokio::spawn(async move {
                        let _ = wait.await;
                        let _ = reply.send(snapshot);
                    });
                } else {
                    let _ = reply.send(snapshot);
                }
            }
        }
    }

    fn sample_positions(&self) -> Vec<(AttemptId, u64)> {
        self.running
            .iter()
            .map(|(id, attempt)| (*id, attempt.position.load(Ordering::Acquire)))
            .collect()
    }

    /// Folds one `Input` into the engine and executes everything it
    /// causes — including further inline effects those cause — before
    /// returning.
    async fn step(&mut self, input: Input) {
        let now = Instant::now();
        let input = match input {
            Input::LinksChanged(links) => Input::LinksChanged(self.sync_clients(links)),
            other => other,
        };
        let effects = self.state.handle(input, now);
        self.drain(effects.into(), now).await;
    }

    async fn drain(&mut self, mut queue: VecDeque<Effect>, now: Instant) {
        while let Some(effect) = queue.pop_front() {
            self.execute(effect, now, &mut queue).await;
        }
    }

    async fn execute(&mut self, effect: Effect, now: Instant, queue: &mut VecDeque<Effect>) {
        match effect {
            Effect::Probe { file, url, via } => self.spawn_probe(file, url, via),
            Effect::Reresolve { file, source } => self.spawn_reresolve(file, source),
            Effect::DiscardFile {
                file,
                dest_dir,
                name,
            } => {
                // Drop our handle first (Windows can't delete an open file).
                self.open_files.remove(&file);
                let final_path = dest_dir.join(&name);
                let _ = std::fs::remove_file(prepare::part_path_for(&final_path));
                // The reserved placeholder is empty until the file is
                // finalized; never delete anything with content.
                if std::fs::metadata(&final_path).is_ok_and(|m| m.len() == 0) {
                    let _ = std::fs::remove_file(&final_path);
                }
            }
            Effect::StartAttempt {
                id,
                file,
                url,
                range,
                file_offset,
                link,
            } => self.spawn_attempt(id, file, url, range, file_offset, link),
            Effect::AbortAttempt { id, .. } => {
                if let Some(attempt) = self.running.remove(&id) {
                    attempt.cancel.cancel();
                }
            }
            Effect::PrepareFile { file, plan } => {
                let result = tokio::task::spawn_blocking(move || match &plan.resume_final_name {
                    Some(name) => prepare::reopen(&plan, name),
                    None => prepare::prepare(&plan).map(|open| (open, true)),
                })
                .await
                .unwrap_or_else(|error| {
                    Err(StorageError::Io {
                        operation: "prepare (task panicked)",
                        path: String::new(),
                        source: std::io::Error::other(error.to_string()),
                    })
                });
                let reported = match result {
                    Ok((open, frontiers_valid)) => {
                        let final_name = open
                            .final_path
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        self.open_files.insert(file, open);
                        Ok(crate::engine::PreparedFile {
                            final_name,
                            frontiers_valid,
                        })
                    }
                    Err(error) => Err(error),
                };
                let more = self.state.handle(
                    Input::FilePrepared {
                        file,
                        result: reported,
                    },
                    now,
                );
                queue.extend(more);
            }
            Effect::Checkpoint { package, manifest } => {
                // Handles are captured now (this file is still open); the
                // lane syncs them and writes the manifest in order.
                let files: Vec<_> = self
                    .open_files
                    .values()
                    .filter(|open| open.package == package)
                    .map(|open| (open.file.clone(), open.part_path.clone()))
                    .collect();
                let _ = self.lane.send(LaneJob::Checkpoint {
                    package,
                    files,
                    manifest,
                });
            }
            Effect::Finalize { file } => {
                if let Some(open) = self.open_files.remove(&file) {
                    let _ = self.lane.send(LaneJob::Finalize { file, open });
                } else {
                    // Nothing open to finish; the engine still counts it.
                    let more = self.state.handle(Input::FinalizeDone { file }, now);
                    queue.extend(more);
                }
            }
            Effect::Emit(event) => {
                let _ = self.lane.send(LaneJob::Emit(event));
            }
            Effect::VerifyPackage {
                package,
                dest_dir,
                files,
            } => self.spawn_verify(package, dest_dir, files),
            Effect::Extract { package, spec } => self.spawn_extract(package, spec),
            Effect::SetPackageLimit { package, limit } => {
                self.limits.set_package(package, limit);
            }
            Effect::CancelPost { package } => {
                if let Some(flag) = self.post_cancel.remove(&package) {
                    flag.store(true, Ordering::Relaxed);
                }
            }
            Effect::DeleteFiles { dest_dir, names } => {
                // Awaited, so the deletion has happened by the time the
                // `PackageDone` that follows it is announced.
                let _ = tokio::task::spawn_blocking(move || {
                    for name in names {
                        // Only plain names in the package's directory.
                        if name.contains(['/', '\\']) || name == ".." {
                            continue;
                        }
                        let _ = std::fs::remove_file(dest_dir.join(&name));
                    }
                })
                .await;
            }
            Effect::WakeAt(_) => {
                // The tick cadence already re-checks every block's backoff
                // against `now` (see `Effect::WakeAt`'s doc comment) — no
                // action needed for correctness.
            }
        }
    }

    /// A fresh cancel flag for `package`'s post-processing.
    fn post_flag(
        &mut self,
        package: crate::model::PackageId,
    ) -> Arc<std::sync::atomic::AtomicBool> {
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        if let Some(old) = self.post_cancel.insert(package, flag.clone()) {
            old.store(true, Ordering::Relaxed);
        }
        flag
    }

    /// Verifies a package on the blocking pool, once every rename issued
    /// before it has landed on the lane.
    fn spawn_verify(
        &mut self,
        package: crate::model::PackageId,
        dest_dir: PathBuf,
        files: Vec<crate::verify::VerifyFile>,
    ) {
        let cancel = self.post_flag(package);
        let (reached, wait) = oneshot::channel();
        let _ = self.lane.send(LaneJob::Barrier(reached));
        let input_tx = self.input_tx.clone();
        tokio::spawn(async move {
            let _ = wait.await;
            let result = tokio::task::spawn_blocking(move || {
                crate::verify::verify_package(&dest_dir, &files, &cancel)
            })
            .await
            .unwrap_or_else(|error| {
                Err(crate::verify::VerifyFailure {
                    file: None,
                    corrupt: false,
                    detail: format!("verification task failed: {error}"),
                })
            });
            let _ = input_tx.send(Input::VerifyDone { package, result }).await;
        });
    }

    /// Extracts a package's archive on the blocking pool, announcing
    /// progress (at most ~10 times a second) as it goes.
    fn spawn_extract(
        &mut self,
        package: crate::model::PackageId,
        spec: crate::extract::ExtractSpec,
    ) {
        let cancel = self.post_flag(package);
        let input_tx = self.input_tx.clone();
        let events_tx = self.events_tx.clone();
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                let mut last = Instant::now();
                let mut progress = |p: crate::extract::ExtractProgress| {
                    if last.elapsed() >= std::time::Duration::from_millis(100) {
                        last = Instant::now();
                        let _ = events_tx.send(EngineEvent::ExtractProgress {
                            package,
                            bytes_done: p.bytes_done,
                            bytes_total: p.bytes_total,
                        });
                    }
                };
                crate::extract::extract(&spec, &cancel, &mut progress)
            })
            .await
            .unwrap_or_else(|error| {
                Err(crate::error::ExtractError::Unsupported {
                    detail: format!("extraction task failed: {error}"),
                })
            });
            let result = result.map_err(|error| error.to_string());
            let _ = input_tx.send(Input::ExtractDone { package, result }).await;
        });
    }

    /// Resolves `source` again and probes what it yields, so a signed URL
    /// that expired is replaced by a fresh one (§13).
    fn spawn_reresolve(&self, file: FileId, source: String) {
        let client = self.default_client.clone();
        let config = self.state.config().clone();
        let registry = self.registry.clone();
        let input_tx = self.input_tx.clone();
        tokio::spawn(async move {
            let ctx = crate::resolve::ResolveContext {
                client: client.clone(),
                cancel: CancellationToken::new(),
            };
            let result = match registry.resolve(&source, &ctx).await {
                Ok(links) => match links.into_iter().next() {
                    Some(link) => crate::probe::probe(&client, link.url, &config)
                        .await
                        .map_err(|error| error.to_string()),
                    None => Err("the source no longer resolves to anything".to_string()),
                },
                Err(error) => Err(error.to_string()),
            };
            let _ = input_tx.send(Input::Reresolved { file, result }).await;
        });
    }

    fn spawn_probe(&self, file: FileId, url: url::Url, via: Option<LinkId>) {
        let client = via
            .and_then(|link| self.link_clients.get(&link))
            .map_or(&self.default_client, |held| &held.client)
            .clone();
        let config = self.state.config().clone();
        let input_tx = self.input_tx.clone();
        tokio::spawn(async move {
            let result: Result<_, ProbeError> = crate::probe::probe(&client, url, &config).await;
            let _ = input_tx.send(Input::ProbeDone { file, result }).await;
        });
    }

    /// Up to `MAX_SAMPLES` 16 KiB stretches of already-durable bytes,
    /// spread across the file's blocks — what a validator mismatch is
    /// checked against (§10).
    fn pick_samples(&self, file: FileId) -> Vec<transfer::SampleRegion> {
        let Some(entry) = self.state.file(file) else {
            return Vec::new();
        };
        let Some(layout) = entry.layout else {
            return Vec::new();
        };
        let total = entry.expected_size.unwrap_or(0);
        let eligible: Vec<(u64, u64)> = entry
            .parts
            .iter()
            .enumerate()
            .filter(|(_, part)| part.durable > 0)
            .filter_map(|(index, part)| {
                let start =
                    crate::plan::block_range(&layout, total, u32::try_from(index).ok()?)?.start;
                Some((start, part.durable))
            })
            .collect();
        let step = (eligible.len() / transfer::MAX_SAMPLES).max(1);
        eligible
            .into_iter()
            .step_by(step)
            .take(transfer::MAX_SAMPLES)
            .map(|(offset, durable)| transfer::SampleRegion {
                offset,
                len: durable.min(transfer::SAMPLE_LEN),
            })
            .collect()
    }

    fn spawn_attempt(
        &mut self,
        id: AttemptId,
        file: FileId,
        url: url::Url,
        range: crate::model::ByteRange,
        file_offset: u64,
        link: LinkId,
    ) {
        let Some(open) = self.open_files.get(&file) else {
            warn!("driver: StartAttempt for {file:?} with no prepared file — dropping");
            return;
        };
        let package = open.package;
        let client = self
            .link_clients
            .get(&link)
            .map_or(&self.default_client, |held| &held.client)
            .clone();
        let accepted = self
            .state
            .file(file)
            .map(|entry| entry.versions.clone())
            .unwrap_or_default();
        let samples = self.pick_samples(file);
        let confirmed_version = Arc::new(std::sync::Mutex::new(None));
        let expected_size = self.state.file(file).and_then(|entry| entry.expected_size);
        let config = self.state.config();

        let position = Arc::new(AtomicU64::new(file_offset));
        let cancel = CancellationToken::new();
        self.running.insert(
            id,
            RunningAttempt {
                position: position.clone(),
                cancel: cancel.clone(),
            },
        );

        let spec = transfer::AttemptSpec {
            client,
            url,
            range,
            file: open.file.clone(),
            path: open.part_path.clone(),
            expected_size,
            accepted,
            samples,
            confirmed_version: confirmed_version.clone(),
            stall_timeout: config.stall_timeout,
            buffer_bytes: config.write_buffer_bytes,
            throttle: self.limits.throttle_for(package, link),
            position,
            cancel,
        };
        let input_tx = self.input_tx.clone();
        tokio::spawn(async move {
            let ran = transfer::run(spec).await;
            // A relabelled-but-identical file (§10) is recorded even if
            // this attempt then fails for another reason.
            let confirmed = confirmed_version.lock().ok().and_then(|mut v| v.take());
            if let Some(version) = confirmed {
                let _ = input_tx
                    .send(Input::VersionAccepted { file, version })
                    .await;
            }
            if let Some((outcome, position)) = ran {
                if let crate::engine::AttemptOutcome::Failed(error) = &outcome {
                    tracing::debug!(?id, ?link, position, %error, "attempt failed");
                }
                let _ = input_tx
                    .send(Input::AttemptEnded {
                        id,
                        outcome,
                        position,
                    })
                    .await;
            }
        });
    }
}
