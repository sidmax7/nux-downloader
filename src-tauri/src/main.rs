// Hides the console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    linux_webview_workarounds();
    nuxd_app_lib::run();
}

/// WebKitGTK's DMA-BUF renderer fails on a number of GPU, driver and
/// Wayland combinations: the window stays blank or the app exits with
/// "Error 71 (Protocol error) dispatching to Wayland display". Turning it
/// off costs a little rendering speed and works everywhere. Someone who
/// wants it back sets `WEBKIT_DISABLE_DMABUF_RENDERER=0` themselves.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn linux_webview_workarounds() {
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        // SAFETY: called first thing in `main`, before any other thread
        // exists, so nothing can read the environment concurrently.
        unsafe { std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1") };
    }
}

#[cfg(not(target_os = "linux"))]
fn linux_webview_workarounds() {}
