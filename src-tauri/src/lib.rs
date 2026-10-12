//! Tauri shell. Thin by design: IPC commands that call into `nuxd-core`,
//! an event forwarder, a tray icon and the app lifecycle. The download
//! logic lives in the engine; the interface lives in `ui/`.

mod commands;
mod dto;
mod host;
mod settings;
mod tray;

use host::AppState;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tauri::{AppHandle, Manager, RunEvent, WindowEvent};
use tauri_specta::{Builder, collect_commands, collect_events};

/// The command and event list, shared by the app and the bindings export.
fn specta_builder() -> Builder<tauri::Wry> {
    Builder::<tauri::Wry>::new()
        .commands(collect_commands![
            commands::engine_snapshot,
            commands::collect_links,
            commands::enqueue_draft,
            commands::discard_drafts,
            commands::pause,
            commands::resume,
            commands::remove,
            commands::restart_file,
            commands::set_priority,
            commands::move_package,
            commands::set_post_options,
            commands::set_package_limit,
            commands::file_blocks,
            commands::get_settings,
            commands::download_path,
            commands::update_settings,
            commands::restart_engine,
            commands::pick_folder,
            commands::diagnostics_report,
        ])
        .events(collect_events![dto::SnapshotEvent, dto::NoticeEvent])
        .dangerously_cast_bigints_to_number()
}

/// Quits after a graceful engine shutdown: transfers stop, a final
/// checkpoint is written, and interrupted packages resume next launch.
pub fn quit(app: &AppHandle) {
    let Some(state) = app.try_state::<Arc<AppState>>() else {
        app.exit(0);
        return;
    };
    if state.quitting.swap(true, Ordering::SeqCst) {
        return;
    }
    let state = Arc::clone(&state);
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        host::stop(&state).await;
        app.exit(0);
    });
}

/// The data directory: `NUXD_DATA_DIR` if set (tests, portable use),
/// else the platform default shared with the `nuxd` CLI.
fn data_dir() -> std::path::PathBuf {
    std::env::var_os("NUXD_DATA_DIR")
        .map(std::path::PathBuf::from)
        .or_else(nuxd_core::diagnostics::default_data_dir)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

/// A termination signal (`kill`, logout) quits as gracefully as the
/// window's close button does.
#[cfg(unix)]
fn quit_on_signals(app: &AppHandle) {
    use tokio::signal::unix::{SignalKind, signal};
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let (Ok(mut term), Ok(mut int)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) else {
            return;
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
        quit(&app);
    });
}

#[cfg(not(unix))]
fn quit_on_signals(_app: &AppHandle) {}

/// Starts the app.
pub fn run() {
    let builder = specta_builder();
    #[cfg(debug_assertions)]
    if let Err(error) = builder.export(
        specta_typescript::Typescript::default(),
        "../ui/src/bindings.ts",
    ) {
        eprintln!("nuxd: couldn't write the TypeScript bindings: {error}");
    }

    let data_dir = data_dir();
    // Held for the life of the app so buffered log lines reach the disk.
    let _logging = nuxd_core::diagnostics::init_logging(
        &nuxd_core::diagnostics::log_dir(&data_dir),
        cfg!(debug_assertions),
    );
    let state = Arc::new(AppState::new(data_dir));
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            tray::show_window(app);
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .manage(Arc::clone(&state))
        .invoke_handler(builder.invoke_handler())
        .setup(move |app| {
            builder.mount_events(app);
            let handle = app.handle().clone();
            quit_on_signals(&handle);
            let has_tray = tray::install(&handle);
            tauri::async_runtime::block_on(host::start(&handle, &state, false));
            if has_tray && let Some(window) = app.get_webview_window("main") {
                let close_handle = handle.clone();
                let close_state = Arc::clone(&state);
                window.on_window_event(move |event| {
                    if let WindowEvent::CloseRequested { api, .. } = event
                        && close_state.settings().close_to_tray
                        && !close_state.quitting.load(Ordering::SeqCst)
                    {
                        api.prevent_close();
                        if let Some(window) = close_handle.get_webview_window("main") {
                            let _ = window.hide();
                        }
                    }
                });
            }
            Ok(())
        })
        .build(tauri::generate_context!());
    let app = match app {
        Ok(app) => app,
        Err(error) => {
            eprintln!("nuxd: the app failed to start: {error}");
            std::process::exit(1);
        }
    };
    app.run(|app, event| {
        if let RunEvent::ExitRequested { api, .. } = event {
            let quitting = app
                .try_state::<Arc<AppState>>()
                .is_some_and(|state| state.quitting.load(Ordering::SeqCst));
            if !quitting {
                api.prevent_exit();
                quit(app);
            }
        }
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use specta_typescript::Typescript;

    /// The committed `ui/src/bindings.ts` must match the Rust types.
    /// Regenerate with `NUXD_UPDATE_BINDINGS=1 cargo test -p nuxd-app`.
    #[test]
    fn bindings_are_up_to_date() {
        let committed =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../ui/src/bindings.ts");
        let fresh = std::env::temp_dir().join(format!("nuxd-bindings-{}.ts", std::process::id()));
        specta_builder()
            .export(Typescript::default(), &fresh)
            .expect("export bindings");
        let fresh_text = std::fs::read_to_string(&fresh).unwrap();
        std::fs::remove_file(&fresh).unwrap();
        if std::env::var_os("NUXD_UPDATE_BINDINGS").is_some() {
            std::fs::write(&committed, &fresh_text).unwrap();
            return;
        }
        let current = std::fs::read_to_string(&committed).unwrap_or_default();
        assert_eq!(
            current, fresh_text,
            "ui/src/bindings.ts is out of date; run `NUXD_UPDATE_BINDINGS=1 cargo test -p nuxd-app`"
        );
    }
}
