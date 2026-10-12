//! Owns the engine for the app's lifetime: starts it, forwards its events
//! to the window, restarts it, and shuts it down on quit (P6.6).

use crate::dto::{
    LinkDto, NoticeEvent, NoticeLevel, PackageState, SnapshotDto, SnapshotEvent, snapshot_dto,
};
use crate::settings::{self, Settings};
use nuxd_core::engine::EngineEvent;
use nuxd_core::model::ByteRate;
use nuxd_core::net::monitor::{LinkSetup, MonitorSetup};
use nuxd_core::{Engine, PackageDraft};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;
use tauri_specta::Event;
use tokio::sync::RwLock;

/// How often the window gets a fresh snapshot while something moves.
const PUSH_EVERY: Duration = Duration::from_millis(250);
/// How often the engine looks for network changes.
const RESCAN: Duration = Duration::from_secs(2);

pub struct AppState {
    pub data_dir: PathBuf,
    engine: RwLock<Option<Engine>>,
    start_error: Mutex<Option<String>>,
    settings: Mutex<Settings>,
    drafts: Mutex<(u64, HashMap<u64, PackageDraft>)>,
    known_links: Mutex<Vec<LinkDto>>,
    last: Mutex<Option<SnapshotDto>>,
    /// Bumped on every (re)start; an event forwarder of an older
    /// generation stops.
    generation: AtomicU64,
    pub quitting: AtomicBool,
}

pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl AppState {
    #[must_use]
    pub fn new(data_dir: PathBuf) -> Self {
        let settings = settings::load(&data_dir);
        Self {
            data_dir,
            engine: RwLock::new(None),
            start_error: Mutex::new(None),
            settings: Mutex::new(settings),
            drafts: Mutex::new((0, HashMap::new())),
            known_links: Mutex::new(Vec::new()),
            last: Mutex::new(None),
            generation: AtomicU64::new(0),
            quitting: AtomicBool::new(false),
        }
    }

    pub fn settings(&self) -> Settings {
        lock(&self.settings).clone()
    }

    pub fn set_settings(&self, settings: Settings) {
        *lock(&self.settings) = settings;
    }

    /// The running engine, or the reason there is none.
    pub async fn engine(&self) -> Result<Engine, String> {
        match self.engine.read().await.as_ref() {
            Some(engine) => Ok(engine.clone()),
            None => Err(lock(&self.start_error)
                .clone()
                .unwrap_or_else(|| "The download engine isn't running.".to_string())),
        }
    }

    /// Keeps a collected draft until the user enqueues or discards it.
    pub fn stash_draft(&self, draft: PackageDraft) -> u64 {
        let mut drafts = lock(&self.drafts);
        drafts.0 += 1;
        let id = drafts.0;
        drafts.1.insert(id, draft);
        id
    }

    pub fn take_draft(&self, id: u64) -> Option<PackageDraft> {
        lock(&self.drafts).1.remove(&id)
    }

    pub fn discard_drafts(&self) {
        lock(&self.drafts).1.clear();
    }

    /// The latest snapshot pushed to the window, for the tray and for
    /// naming packages in notifications.
    pub fn last_snapshot(&self) -> Option<SnapshotDto> {
        lock(&self.last).clone()
    }

    /// The snapshot the window renders.
    pub async fn snapshot(&self) -> SnapshotDto {
        let engine = match self.engine().await {
            Ok(engine) => engine,
            Err(message) => return SnapshotDto::failed(message),
        };
        build_snapshot(self, &engine).await
    }
}

async fn build_snapshot(state: &AppState, engine: &Engine) -> SnapshotDto {
    let raw = engine.snapshot().await;
    let settings = state.settings();
    let remembered = lock(&state.known_links).clone();
    let dto = snapshot_dto(
        &raw,
        &settings,
        engine.global_limit().map(ByteRate::bytes_per_sec),
        |stats| {
            engine
                .link_limit(stats.link.id)
                .map(ByteRate::bytes_per_sec)
        },
        &remembered,
    );
    // Remember every live link, so a switched-off one can still be shown.
    let mut known = lock(&state.known_links);
    for link in dto.links.iter().filter(|link| link.enabled) {
        match known.iter_mut().find(|old| old.name == link.name) {
            Some(old) => *old = link.clone(),
            None => known.push(link.clone()),
        }
    }
    drop(known);
    *lock(&state.last) = Some(dto.clone());
    dto
}

/// Starts the engine from the saved settings and begins forwarding its
/// events. On failure the reason is kept and shown by the window.
pub async fn start(app: &AppHandle, state: &Arc<AppState>, retry_locked: bool) {
    let settings = state.settings();
    let mut monitor = MonitorSetup::system();
    monitor.interval = RESCAN;
    for (name, pref) in &settings.link_prefs {
        if !pref.enabled {
            monitor.switch.set_enabled(name, false);
        }
    }
    let links = LinkSetup::Monitored(monitor);
    let mut attempt = 0;
    let started = loop {
        let result = Engine::start_with(
            settings.engine_config(),
            Some(state.data_dir.clone()),
            links.clone(),
        )
        .await;
        // After a restart the previous engine's lock is released a moment
        // after its last clone is dropped.
        match result {
            Err(nuxd_core::error::EngineError::Storage(
                nuxd_core::error::StorageError::Locked { .. },
            )) if retry_locked && attempt < 20 => {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            other => break other,
        }
    };
    match started {
        Ok(engine) => {
            *lock(&state.start_error) = None;
            let generation = state.generation.fetch_add(1, Ordering::SeqCst) + 1;
            *state.engine.write().await = Some(engine.clone());
            apply_link_prefs(state, &engine).await;
            tauri::async_runtime::spawn(forward(
                app.clone(),
                Arc::clone(state),
                engine,
                generation,
            ));
        }
        Err(error) => {
            *lock(&state.start_error) = Some(error.to_string());
            let _ = SnapshotEvent(SnapshotDto::failed(error.to_string())).emit(app);
        }
    }
}

/// Stops the engine (taking a final checkpoint) and starts a new one.
pub async fn restart(app: &AppHandle, state: &Arc<AppState>) {
    stop(state).await;
    start(app, state, true).await;
}

/// Stops the engine, leaving interrupted packages to resume next start.
pub async fn stop(state: &AppState) {
    state.generation.fetch_add(1, Ordering::SeqCst);
    let engine = state.engine.write().await.take();
    if let Some(engine) = engine {
        engine.shutdown().await;
    }
}

/// Reapplies the user's per-link choices (limits, on/off) to the engine.
pub async fn apply_link_prefs(state: &AppState, engine: &Engine) {
    let settings = state.settings();
    for (name, pref) in &settings.link_prefs {
        engine.set_link_enabled(name, pref.enabled);
    }
    let raw = engine.snapshot().await;
    for stats in &raw.links {
        if let Some(pref) = settings.link_prefs.get(&stats.link.name) {
            engine.set_link_limit(stats.link.id, pref.limit.and_then(ByteRate::new));
        }
    }
}

fn busy(snapshot: &SnapshotDto) -> bool {
    snapshot.packages.iter().any(|package| {
        matches!(
            package.state,
            PackageState::Downloading | PackageState::Verifying | PackageState::Extracting
        )
    }) || snapshot.links.iter().any(|link| link.speed > 0)
}

fn notify(app: &AppHandle, state: &AppState, event: &EngineEvent) {
    let settings = state.settings();
    if !settings.notifications {
        return;
    }
    let name_of = |id: nuxd_core::model::PackageId| {
        state
            .last_snapshot()
            .and_then(|snapshot| {
                snapshot
                    .packages
                    .into_iter()
                    .find(|package| package.id == id.get())
            })
            .map_or_else(|| "A download".to_string(), |package| package.name)
    };
    let (title, body) = match event {
        EngineEvent::PackageDone { package } => ("Download finished", name_of(*package)),
        EngineEvent::PackageFailed { package, error } => {
            ("Download failed", format!("{}: {error}", name_of(*package)))
        }
        EngineEvent::PackagePausedOnError { package, error } => {
            ("Download paused", format!("{}: {error}", name_of(*package)))
        }
        _ => return,
    };
    let _ = app.notification().builder().title(title).body(body).show();
}

/// Pushes snapshots to the window: at 4 Hz while anything moves, and
/// right after any event. Stops when a newer engine generation starts.
async fn forward(app: AppHandle, state: Arc<AppState>, engine: Engine, generation: u64) {
    let mut events = engine.subscribe();
    let mut tick = tokio::time::interval(PUSH_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut dirty = true;
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok(event) => {
                    dirty = true;
                    if matches!(event, EngineEvent::LinksChanged) {
                        apply_link_prefs(&state, &engine).await;
                    }
                    notify(&app, &state, &event);
                    if let EngineEvent::PackageFailed { error, .. }
                        | EngineEvent::PackagePausedOnError { error, .. } = &event
                    {
                        let _ = NoticeEvent {
                            level: NoticeLevel::Error,
                            message: error.to_string(),
                        }
                        .emit(&app);
                    }
                }
                // Fell behind: the snapshot below is the repair.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => dirty = true,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            },
            _ = tick.tick() => {
                if state.generation.load(Ordering::SeqCst) != generation {
                    return;
                }
                let snapshot = build_snapshot(&state, &engine).await;
                if dirty || busy(&snapshot) {
                    dirty = false;
                    let _ = SnapshotEvent(snapshot).emit(&app);
                }
            }
        }
    }
}
