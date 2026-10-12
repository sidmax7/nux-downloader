//! In-app updates (P9.5) from GitHub Releases, signed with the project's
//! own update key. Only offered where the app can replace itself: the
//! Windows installer, the macOS app, and the Linux AppImage. A `.deb`,
//! `.rpm` or AUR install is updated by the package manager instead, and a
//! build without the `self-update` feature never checks.

use crate::dto::UpdateDto;
use crate::host::AppState;
use std::sync::Arc;
use tauri::AppHandle;

/// Whether this copy of the app can update itself.
#[must_use]
pub fn supported() -> bool {
    if !cfg!(feature = "self-update") {
        return false;
    }
    // On Linux only the AppImage replaces itself; packages belong to the
    // package manager.
    !cfg!(target_os = "linux") || std::env::var_os("APPIMAGE").is_some()
}

#[cfg(feature = "self-update")]
pub async fn check(app: &AppHandle) -> Result<Option<UpdateDto>, String> {
    use tauri_plugin_updater::UpdaterExt;
    if !supported() {
        return Ok(None);
    }
    let updater = app.updater().map_err(|e| e.to_string())?;
    let update = updater.check().await.map_err(|e| e.to_string())?;
    Ok(update.map(|update| UpdateDto {
        version: update.version.clone(),
        notes: update.body.clone(),
    }))
}

#[cfg(not(feature = "self-update"))]
pub async fn check(_app: &AppHandle) -> Result<Option<UpdateDto>, String> {
    Ok(None)
}

/// Downloads and installs the update, then restarts. The engine is
/// stopped first, so every download is checkpointed and resumes in the
/// new version.
#[cfg(feature = "self-update")]
pub async fn install(app: &AppHandle, state: &Arc<AppState>) -> Result<(), String> {
    use tauri_plugin_updater::UpdaterExt;
    if !supported() {
        return Err("This copy is updated by your package manager.".into());
    }
    let updater = app.updater().map_err(|e| e.to_string())?;
    let Some(update) = updater.check().await.map_err(|e| e.to_string())? else {
        return Err("You already have the latest version.".into());
    };
    let bytes = update
        .download(|_, _| {}, || {})
        .await
        .map_err(|e| format!("couldn't download the update: {e}"))?;
    crate::host::stop(state).await;
    update
        .install(bytes)
        .map_err(|e| format!("couldn't install the update: {e}"))?;
    app.restart();
}

#[cfg(not(feature = "self-update"))]
pub async fn install(_app: &AppHandle, _state: &Arc<AppState>) -> Result<(), String> {
    Err("This build does not update itself.".into())
}

/// Checks once, a little after launch, when the user allows it; an
/// available update is announced to the window.
pub fn check_in_background(app: &AppHandle, state: &Arc<AppState>) {
    if !supported() || !state.settings().check_updates {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        match check(&app).await {
            Ok(Some(update)) => {
                use tauri_specta::Event;
                let _ = crate::dto::UpdateEvent(update).emit(&app);
            }
            Ok(None) => {}
            Err(error) => eprintln!("nuxd: update check failed: {error}"),
        }
    });
}
