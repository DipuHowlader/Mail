//! Tauri shell for Novamail.
//!
//! One OS process per app, exactly like the desktop runtime build:
//!
//! * **launcher** (default) — the dashboard: license gate, app manager, and the
//!   supervisor that spawns and tracks app processes.
//! * **app** (`--novamail-role=app --novamail-instance=<id>`) — one independent
//!   app with its own config, session and (later) browser.
//!
//! State that used to live in the desktop runtime main process now lives in Rust
//! modules behind the same `window.api.*` surface the React frontend already
//! calls (`src/services/tauriBridge.js` maps the channels).

mod automation;
mod browser;
mod commands;
mod control;
mod documents;
mod files;
mod geo;
mod launcher;
mod license;
mod office_parts;
mod persona;
mod runtime;
mod selftest;
mod store;
mod template;

use std::sync::{Arc, Mutex};

use tauri::Manager;

use crate::browser::BrowserService;
use crate::launcher::{LauncherState, Registry};
use crate::store::Store;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Headless self-test: no window, no message loop — just check the native
    // pieces (device id, config store, license service) and exit.
    if std::env::var("NOVAMAIL_SELFTEST").as_deref() == Ok("1") {
        let mode = std::env::var("NOVAMAIL_SELFTEST_MODE").unwrap_or_default();
        // Modes that don't need the Tauri builder — run them directly.
        if mode == "registry" {
            std::process::exit(selftest::run_registry());
        }
        // Modes that need the Tauri builder — skip the main self-test
        // and let the setup closure handle them.
        if !matches!(
            mode.as_str(),
            "browser"
                | "document"
                | "automation"
                | "gmail"
                | "gmail-live"
                | "isolation"
                | "survival"
                | "render"
                | "launcher"
        ) {
            std::process::exit(selftest::run());
        }
    }

    let info = runtime::RuntimeInfo::from_env();
    runtime::set_process_app_user_model_id(&info);
    let launcher_state: LauncherState = Arc::new(Mutex::new(Registry::new()));
    let app_store = Store::open_at(runtime::store_directory(&info));
    let logger = tauri_plugin_log::Builder::default().level(log::LevelFilter::Info);
    let logger = if info.is_launcher {
        logger
    } else {
        logger.clear_targets().target(tauri_plugin_log::Target::new(
            tauri_plugin_log::TargetKind::Stdout,
        ))
    };

    tauri::Builder::default()
        .plugin(logger.build())
        // Chromium/CDP libraries log at TRACE; keep the app's own logs but
        // keep that flood out of the console and log file.
        .manage(Mutex::new(app_store))
        .manage(launcher_state.clone())
        .manage(BrowserService::new())
        .manage(automation::AutomationService::new())
        .invoke_handler(tauri::generate_handler![
            commands::window_minimize,
            commands::window_maximize,
            commands::window_close,
            commands::window_is_maximized,
            commands::license_check_stored,
            commands::license_verify,
            commands::campaigns_list,
            commands::campaigns_save,
            commands::campaigns_delete,
            files::recipients_pick_and_parse,
            files::templates_pick,
            files::subjects_pick,
            files::attachments_pick,
            documents::document_convert,
            launcher::launcher_list,
            launcher::launcher_create,
            launcher::launcher_open,
            launcher::launcher_close,
            launcher::launcher_restart,
            launcher::launcher_remove,
            launcher::launcher_rename,
            launcher::launcher_focus,
            launcher::launcher_logs,
            launcher::launcher_info,
            automation::automation_add_profile,
            automation::automation_list_profiles,
            automation::automation_list_runs,
            automation::automation_start,
            automation::automation_retry,
            automation::automation_stop,
            automation::automation_pause,
            automation::automation_resume,
            automation::automation_close_profile,
            automation::automation_transmission_state,
            automation::automation_reset_all_instances,
            automation::app_reset_data,
        ])
        .setup(move |app| {
            runtime::create_main_window(app.handle(), &info)?;

            // Headless browser self-test: drive the vendored Chromium through
            // the real driver, then exit with the verdict.
            if std::env::var("NOVAMAIL_SELFTEST_MODE").as_deref() == Ok("browser") {
                tauri::async_runtime::spawn(async move {
                    std::process::exit(selftest::run_browser().await);
                });
                return Ok(());
            }

            // Headless humanized-input self-test: drive the vendored Chromium
            // with the automation humanization enabled and verify the typed
            // path end-to-end, then exit with the verdict.
            if std::env::var("NOVAMAIL_SELFTEST_MODE").as_deref() == Ok("typing") {
                tauri::async_runtime::spawn(async move {
                    std::process::exit(selftest::run_typing().await);
                });
                return Ok(());
            }

            // Headless document-engine self-test: run every supported export
            // format through the real command, PDF included.
            if std::env::var("NOVAMAIL_SELFTEST_MODE").as_deref() == Ok("document") {
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    std::process::exit(selftest::run_documents(handle).await);
                });
                return Ok(());
            }

            // Opt-in performance run for the native document conversion path.
            if std::env::var("NOVAMAIL_SELFTEST_MODE").as_deref() == Ok("document-benchmark") {
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    match documents::run_benchmark(handle).await {
                        Ok(report) => {
                            println!("{report}");
                            std::process::exit(0);
                        }
                        Err(error) => {
                            eprintln!("Tauri document benchmark failed: {error}");
                            std::process::exit(1);
                        }
                    }
                });
                return Ok(());
            }

            let state = launcher_state;
            if let Ok(mut registry) = state.lock() {
                registry.attach(app.handle().clone());
            }

            // Pre-warm the render browser so the first document conversion
            // doesn't pay the Chromium cold-start cost. `BrowserService` is not
            // `Clone`, so hand the task an owned `AppHandle` and resolve the
            // managed state inside it instead of leaking an `app` borrow.
            //
            // Skip the launcher: it never renders documents, so pre-warming
            // there would spawn an idle Chromium (~200-500 MB) for nothing.
            if !info.is_launcher {
                if app.try_state::<BrowserService>().is_some() {
                    let handle = app.handle().clone();
                    tauri::async_runtime::spawn(async move {
                        if let Some(browser) = handle.try_state::<BrowserService>() {
                            browser.prewarm().await;
                        }
                    });
                }
            }

            // The automation state needs the app handle before any background
            // task (Gmail login detection) can emit telemetry.
            if let Some(service) = app.try_state::<automation::AutomationService>() {
                service.attach_app(app.handle());
            }

            // Headless automation self-test: drive the run/worker queue with a
            // deterministic sender, then exit with the verdict.
            if std::env::var("NOVAMAIL_SELFTEST_MODE").as_deref() == Ok("automation") {
                tauri::async_runtime::spawn(async move {
                    std::process::exit(selftest::run_automation().await);
                });
                return Ok(());
            }

            // Headless Gmail-pipeline self-test: exercise the browser-free half
            // of the send pipeline (selectors, formatting, sanitizer), then exit
            // with the verdict.
            if std::env::var("NOVAMAIL_SELFTEST_MODE").as_deref() == Ok("gmail") {
                std::process::exit(selftest::run_gmail());
            }

            // Live Gmail end-to-end diagnostic: launch the real browser, let the
            // operator sign in, then probe (and optionally run) a real send.
            if std::env::var("NOVAMAIL_SELFTEST_MODE").as_deref() == Ok("gmail-live") {
                tauri::async_runtime::spawn(async move {
                    std::process::exit(selftest::run_gmail_live().await);
                });
                return Ok(());
            }

            // Headless launcher render check: boot the window and verify.
            if std::env::var("NOVAMAIL_SELFTEST_MODE").as_deref() == Ok("render") {
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    std::process::exit(selftest::run_render(handle).await);
                });
                return Ok(());
            }

            // Headless app-list persistence check.
            if std::env::var("NOVAMAIL_SELFTEST_MODE").as_deref() == Ok("registry") {
                std::process::exit(selftest::run_registry());
            }

            if info.is_launcher {
                // Start the control channel before any app can be created, then
                // the reaper that notices app exits.
                let (port, token) = control::start_server(state.clone())?;
                if let Ok(mut registry) = state.lock() {
                    registry.set_control(port, token);
                }
                launcher::start_reaper(state.clone());

                // Headless multi-app fault-isolation self-test: create N apps,
                // kill one, verify the survivors, restart, and shut down.
                if std::env::var("NOVAMAIL_SELFTEST_MODE").as_deref() == Ok("isolation") {
                    let state = state.clone();
                    tauri::async_runtime::spawn(async move {
                        std::process::exit(selftest::run_isolation(state).await);
                    });
                    return Ok(());
                }

                // Headless launcher-crash survival self-test: create N apps,
                // print their PIDs, then stay alive for the harness to kill.
                if std::env::var("NOVAMAIL_SELFTEST_MODE").as_deref() == Ok("survival") {
                    let state = state.clone();
                    tauri::async_runtime::spawn(async move {
                        std::process::exit(selftest::run_survival(state).await);
                    });
                    return Ok(());
                }

                // Headless supervisor self-test: create an app, wait for `ready`,
                // then shut it down and exit with the verdict.
                if std::env::var("NOVAMAIL_SELFTEST_MODE").as_deref() == Ok("launcher") {
                    let state = state.clone();
                    tauri::async_runtime::spawn(async move {
                        std::process::exit(selftest::run_launcher(state).await);
                    });
                }
            } else if let (Ok(port), Ok(token)) = (
                std::env::var("NOVAMAIL_CONTROL_PORT"),
                std::env::var("NOVAMAIL_CONTROL_TOKEN"),
            ) {
                if let Ok(port) = port.parse::<u16>() {
                    control::start_client(
                        app.handle().clone(),
                        port,
                        token,
                        info.instance_id.clone(),
                    );
                }
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Novamail");
}
