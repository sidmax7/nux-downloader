//! Tray icon: show the window, pause or resume everything, quit.

use crate::host::AppState;
use nuxd_core::Target;
use std::sync::Arc;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager};

pub fn show_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// Builds the tray icon. Returns whether it exists: the "close to tray"
/// setting is only honoured when it does (some desktops have no tray).
pub fn install(app: &AppHandle) -> bool {
    match build(app) {
        Ok(()) => true,
        Err(error) => {
            eprintln!("nuxd: no tray icon: {error}");
            false
        }
    }
}

fn build(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Show Nux Downloader", true, None::<&str>)?;
    let pause = MenuItem::with_id(app, "pause_all", "Pause all", true, None::<&str>)?;
    let resume = MenuItem::with_id(app, "resume_all", "Resume all", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &show,
            &PredefinedMenuItem::separator(app)?,
            &pause,
            &resume,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;
    let mut builder = TrayIconBuilder::with_id("main")
        .tooltip("Nux Downloader")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => show_window(app),
            "pause_all" => run_on_all(app, true),
            "resume_all" => run_on_all(app, false),
            "quit" => crate::quit(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_window(tray.app_handle());
            }
        });
    // The mark alone reads better than the full app tile at tray size.
    match tauri::image::Image::from_bytes(include_bytes!("../icons/tray.png")) {
        Ok(icon) => builder = builder.icon(icon),
        Err(_) => {
            if let Some(icon) = app.default_window_icon() {
                builder = builder.icon(icon.clone());
            }
        }
    }
    builder.build(app)?;
    Ok(())
}

fn run_on_all(app: &AppHandle, pause: bool) {
    let Some(state) = app.try_state::<Arc<AppState>>() else {
        return;
    };
    let state = Arc::clone(&state);
    tauri::async_runtime::spawn(async move {
        if let Ok(engine) = state.engine().await {
            let _ = if pause {
                engine.pause(Target::All).await
            } else {
                engine.resume(Target::All).await
            };
        }
    });
}
