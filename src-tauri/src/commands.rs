//! IPC commands. Each is a thin call into the engine; errors cross the
//! boundary as readable strings.

use crate::dto::{
    AboutDto, BlocksDto, CollectOptionsDto, DraftDto, EnqueueRequest, PriorityDto, SettingsResult,
    SnapshotDto, UpdateDto, blocks_of, draft_dto,
};
use crate::host::{self, AppState};
use crate::settings::Settings;
use nuxd_core::model::{ByteRate, FileId, PackageId};
use nuxd_core::{AddOptions, Target};
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{AppHandle, State};
use tauri_plugin_dialog::DialogExt;

type AppResult<T> = Result<T, String>;
type Shared<'a> = State<'a, Arc<AppState>>;

fn target(package_id: Option<u64>) -> Target {
    package_id.map_or(Target::All, |id| Target::Package(PackageId::new(id)))
}

fn rate(limit: Option<u64>) -> Option<ByteRate> {
    limit.and_then(ByteRate::new)
}

fn text<E: std::fmt::Display>(error: E) -> String {
    error.to_string()
}

#[tauri::command]
#[specta::specta]
pub async fn engine_snapshot(state: Shared<'_>) -> AppResult<SnapshotDto> {
    Ok(state.snapshot().await)
}

/// Resolves and probes the pasted links and returns drafts to review;
/// links that are web pages are scanned for files when asked (§13.1).
#[tauri::command]
#[specta::specta]
pub async fn collect_links(
    state: Shared<'_>,
    inputs: Vec<String>,
    options: CollectOptionsDto,
) -> AppResult<Vec<DraftDto>> {
    let engine = state.engine().await?;
    state.discard_drafts();
    let drafts = engine
        .collect_with(
            inputs,
            nuxd_core::collect::CollectOptions {
                scan_pages: options.scan_pages,
                deep: options.deep,
            },
        )
        .await;
    Ok(drafts
        .into_iter()
        .map(|draft| {
            let dto_source = draft.clone();
            let id = state.stash_draft(draft);
            draft_dto(id, &dto_source)
        })
        .collect())
}

/// Queues a reviewed draft; returns the new package's id.
#[tauri::command]
#[specta::specta]
pub async fn enqueue_draft(state: Shared<'_>, request: EnqueueRequest) -> AppResult<u64> {
    let engine = state.engine().await?;
    let mut draft = state
        .take_draft(request.draft_id)
        .ok_or("That draft is no longer available; add the links again.")?;
    if let Some(keep) = &request.files {
        draft.files.retain(|file| keep.contains(&file.source));
        if draft.files.is_empty() {
            return Err("Pick at least one file to add.".into());
        }
    }
    let name = request.name.trim();
    if name.is_empty() {
        return Err("The package needs a name.".into());
    }
    let dest_dir = match request.dest_dir.as_deref().map(str::trim) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => state.settings().download_path(),
    };
    let opts = AddOptions {
        name: name.to_string(),
        dest_dir,
        priority: request.priority.into(),
        extract: request.extract && draft.offers_extract(),
        delete_archives: request.delete_archives,
        own_folder: state.settings().package_folders,
    };
    engine
        .enqueue(draft, opts)
        .await
        .map(|id| id.get())
        .map_err(text)
}

#[tauri::command]
#[specta::specta]
pub async fn discard_drafts(state: Shared<'_>) -> AppResult<()> {
    state.discard_drafts();
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn pause(state: Shared<'_>, package_id: Option<u64>) -> AppResult<()> {
    state
        .engine()
        .await?
        .pause(target(package_id))
        .await
        .map_err(text)
}

#[tauri::command]
#[specta::specta]
pub async fn resume(state: Shared<'_>, package_id: Option<u64>) -> AppResult<()> {
    state
        .engine()
        .await?
        .resume(target(package_id))
        .await
        .map_err(text)
}

#[tauri::command]
#[specta::specta]
pub async fn remove(
    state: Shared<'_>,
    package_id: Option<u64>,
    delete_files: bool,
) -> AppResult<()> {
    state
        .engine()
        .await?
        .remove(target(package_id), delete_files)
        .await
        .map_err(text)
}

#[tauri::command]
#[specta::specta]
pub async fn restart_file(state: Shared<'_>, file_id: u64) -> AppResult<()> {
    state
        .engine()
        .await?
        .restart_file(FileId::new(file_id))
        .await
        .map_err(text)
}

#[tauri::command]
#[specta::specta]
pub async fn set_priority(
    state: Shared<'_>,
    package_id: u64,
    priority: PriorityDto,
) -> AppResult<()> {
    state
        .engine()
        .await?
        .set_priority(PackageId::new(package_id), priority.into())
        .await
        .map_err(text)
}

#[tauri::command]
#[specta::specta]
pub async fn move_package(state: Shared<'_>, package_id: u64, position: u32) -> AppResult<()> {
    state
        .engine()
        .await?
        .move_to(PackageId::new(package_id), position as usize)
        .await
        .map_err(text)
}

#[tauri::command]
#[specta::specta]
pub async fn set_post_options(
    state: Shared<'_>,
    package_id: u64,
    extract: bool,
    delete_archives: bool,
) -> AppResult<()> {
    state
        .engine()
        .await?
        .set_post_options(PackageId::new(package_id), extract, delete_archives)
        .await
        .map_err(text)
}

/// `None` removes the package's speed limit.
#[tauri::command]
#[specta::specta]
pub async fn set_package_limit(
    state: Shared<'_>,
    package_id: u64,
    limit: Option<u64>,
) -> AppResult<()> {
    state
        .engine()
        .await?
        .set_package_limit(PackageId::new(package_id), rate(limit))
        .await
        .map_err(text)
}

/// The block grid of one file.
#[tauri::command]
#[specta::specta]
pub async fn file_blocks(state: Shared<'_>, package_id: u64, file_id: u64) -> AppResult<BlocksDto> {
    let snapshot = state.engine().await?.snapshot().await;
    snapshot
        .packages
        .iter()
        .find(|p| p.id.get() == package_id)
        .and_then(|p| p.files.iter().find(|f| f.id.get() == file_id))
        .map(blocks_of)
        .ok_or_else(|| "That file is no longer in the queue.".to_string())
}

#[tauri::command]
#[specta::specta]
pub fn get_settings(state: Shared<'_>) -> Settings {
    state.settings()
}

/// Where new downloads go right now.
#[tauri::command]
#[specta::specta]
pub fn download_path(state: Shared<'_>) -> String {
    state.settings().download_path().display().to_string()
}

/// Validates, saves and applies new settings. Limits and link choices
/// take effect at once; the rest says `restart_needed`.
#[tauri::command]
#[specta::specta]
pub async fn update_settings(state: Shared<'_>, settings: Settings) -> AppResult<SettingsResult> {
    settings.validate()?;
    let old = state.settings();
    crate::settings::save(&state.data_dir, &settings)?;
    let restart_needed = old.needs_restart_for(&settings);
    state.set_settings(settings.clone());
    if let Ok(engine) = state.engine().await {
        engine.set_global_limit(rate(settings.global_limit));
        // A link that lost its entry goes back to the defaults.
        for name in old.link_prefs.keys() {
            if !settings.link_prefs.contains_key(name) {
                engine.set_link_enabled(name, true);
            }
        }
        let snapshot = engine.snapshot().await;
        for stats in &snapshot.links {
            if !settings.link_prefs.contains_key(&stats.link.name) {
                engine.set_link_limit(stats.link.id, None);
            }
        }
        host::apply_link_prefs(&state, &engine).await;
    }
    Ok(SettingsResult {
        settings,
        restart_needed,
    })
}

/// Stops the engine (a final checkpoint is taken) and starts it again
/// with the saved settings.
#[tauri::command]
#[specta::specta]
pub async fn restart_engine(app: AppHandle, state: Shared<'_>) -> AppResult<()> {
    let state = Arc::clone(&state);
    host::restart(&app, &state).await;
    match state.engine().await {
        Ok(_) => Ok(()),
        Err(message) => Err(message),
    }
}

/// Opens the system folder chooser; `None` if the user cancelled.
#[tauri::command]
#[specta::specta]
pub async fn pick_folder(app: AppHandle) -> Option<String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog().file().pick_folder(move |path| {
        let _ = tx.send(path.map(|path| path.to_string()));
    });
    rx.await.ok().flatten()
}

/// A plain-text report for bug reports; nothing is sent anywhere.
#[tauri::command]
#[specta::specta]
pub async fn diagnostics_report(state: Shared<'_>) -> AppResult<String> {
    Ok(state.engine().await?.diagnostics().await)
}

/// Version and update capability, for Settings → About.
#[tauri::command]
#[specta::specta]
pub fn about(app: AppHandle) -> AboutDto {
    AboutDto {
        version: app.package_info().version.to_string(),
        can_self_update: crate::update::supported(),
    }
}

/// Asks GitHub Releases for a newer version.
#[tauri::command]
#[specta::specta]
pub async fn check_for_update(app: AppHandle) -> AppResult<Option<UpdateDto>> {
    crate::update::check(&app).await
}

/// Installs the newer version and restarts into it.
#[tauri::command]
#[specta::specta]
pub async fn install_update(app: AppHandle, state: Shared<'_>) -> AppResult<()> {
    let state = Arc::clone(&state);
    crate::update::install(&app, &state).await
}

/// The licences of everything the app is built from.
#[tauri::command]
#[specta::specta]
pub fn third_party_notices() -> String {
    include_str!("../../THIRD_PARTY_NOTICES.md").to_string()
}
