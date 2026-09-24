//! The public API (`ARCHITECTURE.md` §15): `Engine`, the type `cli`
//! (P2.9) and, later, `src-tauri` build on. Thin — it owns the on-disk
//! queue manifest and the data directory, and otherwise just forwards to
//! a `Driver`.

use crate::config::EngineConfig;
use crate::driver::Driver;
use crate::engine::{Command, EngineEvent, EngineSnapshot, EngineState};
use crate::error::{EngineError, StorageError};
use crate::model::{Link, LinkId, LinkKind, PackageId, Priority};
use crate::net::bind::BindPolicy;
use crate::net::monitor::LinkSetup;
use crate::storage::manifest::{
    QUEUE_SCHEMA_VERSION, QueueManifest, QueueSettings, load_all_package_manifests,
    load_queue_manifest, save_queue_manifest,
};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::broadcast;

/// Where a new package goes, and at what priority. Mirrors §15's
/// `AddOptions`.
#[derive(Debug, Clone)]
pub struct AddOptions {
    pub name: String,
    pub dest_dir: PathBuf,
    pub priority: Priority,
}

/// Which package(s) a `pause`/`resume`/`cancel`/`remove` call applies to.
///
/// §15 also has a `Target::File` for per-file control; that's not built
/// yet — `Command` has no per-file pause or cancel in M1 (see P2.6's
/// scoping notes), so there is nothing for it to reach. It's added once
/// a later phase gives individual files their own commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    All,
    Package(PackageId),
}

/// The engine's public handle. Cloning it is cheap and shares the same
/// running driver (`Driver` is itself `Clone`), though most programs only
/// ever need one.
#[derive(Clone)]
pub struct Engine {
    driver: Driver,
    manifest_dir: PathBuf,
    queue_path: PathBuf,
}

/// The single synthetic link `Engine::start` uses (`ARCHITECTURE.md`
/// §6.1). It carries no address and its client is unbound, so the OS
/// routes every connection, same as the default client probes use.
fn default_link() -> Link {
    Link {
        id: LinkId::new(1),
        name: "default".to_string(),
        label: "Default".to_string(),
        kind: LinkKind::Other,
        v4: None,
        v6: None,
        max_slots: 8,
        limit: None,
    }
}

fn io_error(
    operation: &'static str,
    path: &std::path::Path,
    source: std::io::Error,
) -> StorageError {
    StorageError::Io {
        operation,
        path: path.display().to_string(),
        source,
    }
}

impl Engine {
    /// Starts the engine: loads `queue.json` and every package manifest
    /// from `data_dir` (or the platform default, via `directories`, if
    /// `None`), restores engine state from them, and spawns the driver.
    /// Packages that were `Downloading` resume automatically unless
    /// `config.auto_resume_on_startup` is off (§9.4, rule 9).
    ///
    /// Uses one synthetic link and unbound clients — the OS routes every
    /// connection. `start_with` chooses real, monitored links instead.
    pub async fn start(
        config: EngineConfig,
        data_dir: Option<PathBuf>,
    ) -> Result<Self, EngineError> {
        Self::start_with(
            config,
            data_dir,
            LinkSetup::Fixed {
                links: vec![default_link()],
                bind: BindPolicy::Off,
            },
        )
        .await
    }

    /// `start`, with the links coming from `links`: a fixed list, or
    /// `LinkSetup::Monitored(MonitorSetup::system())` to discover the
    /// machine's interfaces, bind each one, and follow changes (§6.1).
    pub async fn start_with(
        config: EngineConfig,
        data_dir: Option<PathBuf>,
        links: LinkSetup,
    ) -> Result<Self, EngineError> {
        let data_dir = match data_dir {
            Some(dir) => dir,
            None => {
                let dirs = directories::ProjectDirs::from("", "", "nuxd").ok_or_else(|| {
                    StorageError::Io {
                        operation: "locate the data directory",
                        path: String::new(),
                        source: std::io::Error::other("no home directory for this platform/user"),
                    }
                })?;
                dirs.data_dir().to_path_buf()
            }
        };
        let manifest_dir = data_dir.join("packages");
        let queue_path = data_dir.join("queue.json");

        std::fs::create_dir_all(&manifest_dir)
            .map_err(|source| io_error("create", &manifest_dir, source))?;

        let queue = if queue_path.exists() {
            load_queue_manifest(&queue_path)?
        } else {
            QueueManifest {
                schema_version: QUEUE_SCHEMA_VERSION,
                package_order: Vec::new(),
                next_package_id: 1,
                next_file_id: 1,
                settings: QueueSettings {
                    auto_resume_on_startup: config.auto_resume_on_startup,
                    destination_dir: None,
                },
            }
        };
        let packages = load_all_package_manifests(&manifest_dir)
            .into_iter()
            .map(|manifest| manifest.package)
            .collect();

        let (state, initial_effects) = EngineState::restore(config, &queue, packages);
        let driver = Driver::spawn_with(state, initial_effects, links, manifest_dir.clone());

        Ok(Self {
            driver,
            manifest_dir,
            queue_path,
        })
    }

    /// Adds one package containing one file per source URL, and returns
    /// its id. §15's signature returns `Vec<PackageId>` for when a batch
    /// of inputs splits into several packages (grouping, §12.1) — that
    /// split doesn't exist before Phase 5, so this always returns exactly
    /// one id for now.
    pub async fn add(
        &self,
        sources: Vec<String>,
        opts: AddOptions,
    ) -> Result<Vec<PackageId>, EngineError> {
        let mut events = self.driver.subscribe();
        self.driver
            .command(Command::AddPackage {
                name: opts.name,
                dest_dir: opts.dest_dir,
                sources,
                priority: opts.priority,
            })
            .await
            .map_err(|_| shutdown_error())?;

        let package = wait_for(&mut events, |event| match event {
            EngineEvent::PackageAdded { package } => Some(*package),
            _ => None,
        })
        .await
        .ok_or_else(shutdown_error)?;

        self.save_queue().await?;
        Ok(vec![package])
    }

    pub async fn pause(&self, target: Target) -> Result<(), EngineError> {
        self.for_target(target, Command::Pause).await
    }

    pub async fn resume(&self, target: Target) -> Result<(), EngineError> {
        self.for_target(target, Command::Resume).await
    }

    pub async fn cancel(&self, target: Target) -> Result<(), EngineError> {
        self.for_target(target, Command::Cancel).await
    }

    /// Removes package(s) from the queue. `delete_files` also deletes
    /// each file's on-disk bytes (and any leftover `.nuxdpart`); either
    /// way, the package's own manifest is deleted, since it no longer
    /// exists to be restored from on a future restart.
    pub async fn remove(&self, target: Target, delete_files: bool) -> Result<(), EngineError> {
        let snapshot = self.snapshot().await;
        let ids: Vec<PackageId> = match target {
            Target::All => snapshot.packages.iter().map(|package| package.id).collect(),
            Target::Package(id) => vec![id],
        };

        for id in ids {
            let package = snapshot.packages.iter().find(|package| package.id == id);
            if delete_files && let Some(package) = package {
                for file in &package.files {
                    let final_path = package.dest_dir.join(&file.name);
                    let _ = std::fs::remove_file(&final_path);
                    let _ =
                        std::fs::remove_file(crate::driver::prepare::part_path_for(&final_path));
                }
            }
            self.driver
                .command(Command::Remove(id))
                .await
                .map_err(|_| shutdown_error())?;
            let _ = std::fs::remove_file(crate::driver::prepare::manifest_path(
                &self.manifest_dir,
                id,
            ));
        }

        self.save_queue().await
    }

    pub async fn snapshot(&self) -> EngineSnapshot {
        self.driver.snapshot().await.unwrap_or(EngineSnapshot {
            packages: Vec::new(),
            next_package_id: 0,
            next_file_id: 0,
            links: Vec::new(),
        })
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.driver.subscribe()
    }

    /// Stops all transfers and takes a final checkpoint; package statuses
    /// are left as they are, so an interrupted package auto-resumes on
    /// the next `start` (§9.4).
    pub async fn shutdown(self) {
        self.driver.shutdown().await;
    }

    async fn for_target(
        &self,
        target: Target,
        command: impl Fn(PackageId) -> Command,
    ) -> Result<(), EngineError> {
        let ids: Vec<PackageId> = match target {
            Target::All => self
                .snapshot()
                .await
                .packages
                .iter()
                .map(|package| package.id)
                .collect(),
            Target::Package(id) => vec![id],
        };
        for id in ids {
            self.driver
                .command(command(id))
                .await
                .map_err(|_| shutdown_error())?;
        }
        Ok(())
    }

    /// Rewrites `queue.json` from the driver's current package order and
    /// id counters (§9.4) — called after anything that changes either.
    async fn save_queue(&self) -> Result<(), EngineError> {
        let snapshot = self.snapshot().await;
        let existing = if self.queue_path.exists() {
            load_queue_manifest(&self.queue_path)?
        } else {
            QueueManifest::default()
        };
        let queue = QueueManifest {
            schema_version: QUEUE_SCHEMA_VERSION,
            package_order: snapshot.packages.iter().map(|package| package.id).collect(),
            next_package_id: snapshot.next_package_id,
            next_file_id: snapshot.next_file_id,
            settings: existing.settings,
        };
        let queue_path = self.queue_path.clone();
        let error_path = queue_path.clone();
        tokio::task::spawn_blocking(move || save_queue_manifest(&queue_path, &queue))
            .await
            .map_err(|error| {
                EngineError::Storage(io_error(
                    "save queue.json (task panicked)",
                    &error_path,
                    std::io::Error::other(error.to_string()),
                ))
            })??;
        Ok(())
    }
}

fn shutdown_error() -> EngineError {
    EngineError::Storage(StorageError::Io {
        operation: "reach the engine",
        path: String::new(),
        source: std::io::Error::other("the engine has already shut down"),
    })
}

/// Waits for the first event `predicate` accepts, bounded so a caller
/// never hangs forever if the driver task is gone or the event was
/// somehow missed.
async fn wait_for<T>(
    events: &mut broadcast::Receiver<EngineEvent>,
    predicate: impl Fn(&EngineEvent) -> Option<T>,
) -> Option<T> {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match events.recv().await {
                Ok(event) => {
                    if let Some(value) = predicate(&event) {
                        return Some(value);
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return None,
                Err(broadcast::error::RecvError::Lagged(_)) => {}
            }
        }
    })
    .await
    .ok()
    .flatten()
}
