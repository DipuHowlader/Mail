//! Shared runtime bootstrap for the Tauri shell.
//!
//! Novamail runs as two very different process roles that share one binary:
//!
//!   - `launcher` — a lightweight supervisor window. It only creates, lists and
//!                  manages app instances. It never runs browser automation,
//!                  telemetry or document rendering.
//!   - `app`      — one independent app instance. It owns its own Gmail session,
//!                  browser profile, telemetry pipeline, task queue and document
//!                  engine. Nothing is shared with other app processes.
//!
//! The role is chosen from `NOVAMAIL_ROLE` / `--novamail-role=` so a child can be
//! launched from the packaged executable or from a dev build identically. This is
//! the Rust equivalent of `desktop-runtime/runtimeInfo.cjs`.

use std::collections::HashSet;
use std::path::PathBuf;

use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

use crate::store::store_dir;

/// The resolved runtime role and instance identity for this process.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeInfo {
    pub role: String,
    pub is_launcher: bool,
    pub instance_id: String,
    pub instance_name: String,
}

fn cli_arg(name: &str) -> Option<String> {
    let prefix = format!("--{name}=");
    std::env::args().find_map(|arg| arg.strip_prefix(&prefix).map(str::to_string))
}

impl RuntimeInfo {
    /// Read the role/identity from the environment or the command line.
    pub fn from_env() -> Self {
        let role = std::env::var("NOVAMAIL_ROLE")
            .ok()
            .or_else(|| cli_arg("novamail-role"))
            .unwrap_or_else(|| "launcher".to_string());
        let instance_id = std::env::var("NOVAMAIL_INSTANCE")
            .ok()
            .or_else(|| cli_arg("novamail-instance"))
            .unwrap_or_default();
        let instance_name = std::env::var("NOVAMAIL_INSTANCE_NAME")
            .ok()
            .or_else(|| cli_arg("novamail-instance-name"))
            .unwrap_or_default();
        let is_launcher = role != "app";
        Self {
            role,
            is_launcher,
            instance_id,
            instance_name,
        }
    }
}

/// Create this process's single window and tell the frontend which role it is.
///
/// The role/identity is injected synchronously through an initialization script
/// so the renderer's `window.api.runtime` is correct before React starts — the
/// same guarantee the desktop runtime preload gave via `additionalArguments`.
pub fn create_main_window(app: &AppHandle, info: &RuntimeInfo) -> tauri::Result<()> {
    let (width, height, min_width, min_height) = if info.is_launcher {
        (1040.0, 760.0, 900.0, 620.0)
    } else {
        (1440.0, 900.0, 1024.0, 700.0)
    };

    let title = if info.is_launcher {
        "Novamail".to_string()
    } else if info.instance_name.trim().is_empty() {
        "Novamail App".to_string()
    } else {
        info.instance_name.clone()
    };

    let runtime_json = serde_json::to_string(info).unwrap_or_else(|_| "{}".to_string());
    let init_script = format!("window.__NOVAMAIL_RUNTIME__ = {runtime_json};");

    // Headless self-tests create real windows but keep them off-screen.
    let hidden = std::env::var("NOVAMAIL_WINDOW_HIDDEN").as_deref() == Ok("1");
    let data_directory = webview_data_directory(app, info)?;

    let window = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
        .data_directory(data_directory)
        .title(title)
        .inner_size(width, height)
        .min_inner_size(min_width, min_height)
        // The app draws its own title bar (see src/components/TitleBar.jsx).
        .decorations(false)
        .visible(!hidden)
        .initialization_script(&init_script[..])
        .build()?;

    // Apply the configured Novamail icon directly to the native window so the
    // Windows taskbar uses the product icon instead of the old Tauri default.
    let icon = match app.default_window_icon() {
        Some(icon) => icon.clone(),
        None => tauri::image::Image::from_bytes(include_bytes!("../icons/icon.png"))?,
    };
    window.set_icon(icon)?;

    // Mirror desktop runtime's `window:state` events so the renderer's maximize button
    // stays in sync when the user maximizes by dragging or double-clicking.
    let handle = window.clone();
    let app_handle = app.clone();
    let is_app_window = !info.is_launcher;
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
            if is_app_window {
                api.prevent_close();
                let app = app_handle.clone();
                tauri::async_runtime::spawn(async move {
                    shutdown_app(&app).await;
                });
                return;
            }
        }
        if let tauri::WindowEvent::Resized(_) = event {
            let maximized = handle.is_maximized().unwrap_or(false);
            let _ = handle.emit(
                "window:state",
                serde_json::json!({ "maximized": maximized }),
            );
        }
    });

    Ok(())
}

/// Close this app's automation browsers before exiting the process.
///
/// Both the native window close event and the launcher's shutdown command use
/// this path so a manually closed or removed app cannot leave Chromium behind.
pub async fn shutdown_app(app: &AppHandle) {
    let info = RuntimeInfo::from_env();
    if let Some(service) = app.try_state::<crate::automation::AutomationService>() {
        service.shutdown_all().await;
    }

    if !info.is_launcher {
        if let Some(store) = app.try_state::<std::sync::Mutex<crate::store::Store>>() {
            if let Ok(mut store) = store.lock() {
                if let Err(error) = store.clear() {
                    log::warn!("Could not clear app-instance storage: {error}");
                }
            }
        }
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.destroy();
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        cleanup_instance_data(app, &info.instance_id, Some(std::process::id()));
    }
    app.exit(0);
}

/// Root for one disposable app instance's store and WebView2 profile.
pub fn instance_data_directory(instance_id: &str) -> PathBuf {
    store_dir()
        .join("instances")
        .join(safe_instance_id(instance_id))
}

/// Store directory used by the child app process. Launcher-owned license and
/// app registry state remain in the shared store; app content is isolated and
/// removed with its instance.
pub fn store_directory(info: &RuntimeInfo) -> PathBuf {
    if info.is_launcher {
        store_dir()
    } else {
        instance_data_directory(&info.instance_id).join("store")
    }
}

/// Remove all files owned by a finished app process. The second root removes
/// WebView2 data left by earlier builds, which stored it under Tauri's default
/// app-data directory.
pub fn cleanup_instance_data(app: &AppHandle, instance_id: &str, process_id: Option<u32>) {
    if !is_app_instance_id(instance_id) {
        return;
    }

    let current_root = instance_data_directory(instance_id);
    let _ = std::fs::remove_dir_all(&current_root);

    if let Ok(app_data) = app.path().app_data_dir() {
        let legacy_root = app_data.join("instances").join(instance_id);
        if legacy_root != current_root {
            let _ = std::fs::remove_dir_all(legacy_root);
            remove_directory_if_empty(&app_data.join("instances"));
        }
    }
    remove_directory_if_empty(&store_dir().join("instances"));

    if let Some(process_id) = process_id {
        let temp = std::env::temp_dir();
        remove_prefixed_entries(&temp, &format!("novamail-session-{process_id}-"));
        remove_prefixed_entries(
            &temp.join("novamail-browser"),
            &format!("profile-{process_id}-"),
        );
        remove_prefixed_entries(&temp, &format!("novamail-send-{process_id}-"));
        remove_prefixed_entries(
            &temp,
            &format!("novamail-tauri-document-benchmark-{process_id}-"),
        );
        remove_prefixed_entries(
            &temp.join("novamail-render"),
            &format!("render-{process_id}-"),
        );
        remove_directory_if_empty(&temp.join("novamail-browser"));
        remove_directory_if_empty(&temp.join("novamail-render"));
    }
}

/// Sweep instance and temporary artifacts left by older or crashed app
/// processes. Keep only directories owned by PIDs that the launcher still sees
/// alive.
pub fn cleanup_stale_instance_data(
    app: &AppHandle,
    active_instances: &HashSet<String>,
    active_process_ids: &HashSet<u32>,
) {
    cleanup_stale_instance_directories(
        &store_dir().join("instances"),
        active_instances,
    );
    if let Ok(app_data) = app.path().app_data_dir() {
        cleanup_stale_instance_directories(&app_data.join("instances"), active_instances);
    }

    let temp = std::env::temp_dir();
    cleanup_stale_pid_entries(&temp, "novamail-session-", active_process_ids);
    cleanup_stale_pid_entries(&temp, "novamail-send-", active_process_ids);
    cleanup_stale_pid_entries(
        &temp,
        "novamail-tauri-document-benchmark-",
        active_process_ids,
    );
    cleanup_stale_pid_entries(
        &temp.join("novamail-browser"),
        "profile-",
        active_process_ids,
    );
    cleanup_stale_pid_entries(
        &temp.join("novamail-render"),
        "render-",
        active_process_ids,
    );
}

fn safe_instance_id(instance_id: &str) -> String {
    let safe: String = instance_id
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        .take(80)
        .collect();
    if safe.is_empty() {
        "unassigned".to_string()
    } else {
        safe
    }
}

fn is_app_instance_id(instance_id: &str) -> bool {
    let Some(suffix) = instance_id.strip_prefix("app_") else {
        return false;
    };
    (4..=64).contains(&suffix.chars().count())
        && suffix
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
}

fn remove_prefixed_entries(directory: &std::path::Path, prefix: &str) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(prefix))
        {
            continue;
        }
        let path = entry.path();
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            let _ = std::fs::remove_dir_all(path);
        } else {
            let _ = std::fs::remove_file(path);
        }
    }
    remove_directory_if_empty(directory);
}

fn remove_directory_if_empty(directory: &std::path::Path) {
    if std::fs::read_dir(directory)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(false)
    {
        let _ = std::fs::remove_dir(directory);
    }
}

fn cleanup_stale_instance_directories(
    directory: &std::path::Path,
    active_instances: &HashSet<String>,
) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let Some(instance_id) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if !is_app_instance_id(&instance_id) || active_instances.contains(&instance_id) {
            continue;
        }
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
    remove_directory_if_empty(directory);
}

fn cleanup_stale_pid_entries(
    directory: &std::path::Path,
    prefix: &str,
    active_process_ids: &HashSet<u32>,
) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Some(suffix) = name.strip_prefix(prefix) else {
            continue;
        };
        let process_id = suffix.split('-').next().and_then(|value| value.parse().ok());
        if process_id
            .map(|pid| active_process_ids.contains(&pid))
            .unwrap_or(!active_process_ids.is_empty())
        {
            continue;
        }
        let path = entry.path();
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            let _ = std::fs::remove_dir_all(path);
        } else {
            let _ = std::fs::remove_file(path);
        }
    }
    remove_directory_if_empty(directory);
}

/// Give each app process an isolated WebView2 profile so cookies, IndexedDB
/// and renderer caches cannot cross app-instance boundaries.
fn webview_data_directory(app: &AppHandle, info: &RuntimeInfo) -> tauri::Result<PathBuf> {
    if info.is_launcher {
        let mut path = app.path().app_data_dir()?;
        path.push("launcher");
        path.push("webview");
        std::fs::create_dir_all(&path)?;
        Ok(path)
    } else {
        let path = instance_data_directory(&info.instance_id).join("webview");
        std::fs::create_dir_all(&path)?;
        Ok(path)
    }
}

/// Keep launcher and each app instance in a separate Windows taskbar group.
pub fn set_process_app_user_model_id(info: &RuntimeInfo) {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        let value = if info.is_launcher {
            "com.ippo.novamail.launcher".to_string()
        } else {
            let suffix: String = info
                .instance_id
                .chars()
                .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'))
                .take(80)
                .collect();
            format!(
                "com.ippo.novamail.app.{}",
                if suffix.is_empty() {
                    "default"
                } else {
                    &suffix
                }
            )
        };
        let wide: Vec<u16> = std::ffi::OsStr::new(&value)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            let _ = windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(
                wide.as_ptr(),
            );
        }
    }
    #[cfg(not(windows))]
    let _ = info;
}
