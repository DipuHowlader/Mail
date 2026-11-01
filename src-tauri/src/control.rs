//! Launcher ↔ app control channel.
//!
//! Replaces desktop runtime's Node IPC pipe (`stdio: 'ipc'`) with a loopback TCP socket
//! guarded by a per-launch random token. The protocol is newline-delimited JSON;
//! see `launcher.rs` for the message catalogue.
//!
//! Design notes:
//!   - the server lives in the launcher and never blocks it: each connection is
//!     its own task, and all registry access is short and synchronous,
//!   - the client lives in an app and reports readiness, Gmail identity,
//!     heartbeats and when its Chromium window closes;
//!     it acts on `shutdown`/`focus`/`rename` commands from the launcher,
//!   - if the launcher dies the socket drops: the app keeps running (isolated),
//!     exactly as it did under desktop runtime when the IPC pipe closed.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use std::{fs, io::Write, path::PathBuf};

use serde_json::{json, Value};
use tauri::{AppHandle, Manager};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

use crate::launcher::{self, LauncherState};
use crate::store::{now_ms, store_dir};

#[derive(Clone)]
struct Endpoint {
    port: u16,
    token: String,
}

#[derive(Default)]
struct AppReporter {
    sender: Option<UnboundedSender<String>>,
    latest_state: Option<String>,
}

static APP_REPORTERS: OnceLock<Mutex<HashMap<String, Arc<Mutex<AppReporter>>>>> = OnceLock::new();

fn app_reporters() -> &'static Mutex<HashMap<String, Arc<Mutex<AppReporter>>>> {
    APP_REPORTERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn app_reporter(instance_id: &str) -> Arc<Mutex<AppReporter>> {
    let mut reporters = app_reporters()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    reporters
        .entry(instance_id.to_string())
        .or_default()
        .clone()
}

/// Report the Gmail address detected by this app's own browser session to the
/// launcher record for the app process that owns it.
pub fn report_gmail_account(email: &str) {
    let instance_id = std::env::var("NOVAMAIL_INSTANCE_ID")
        .ok()
        .filter(|id| !id.is_empty())
        .or_else(|| {
            std::env::args().find_map(|argument| {
                argument
                    .strip_prefix("--novamail-instance=")
                    .map(str::to_string)
            })
        });
    let Some(instance_id) = instance_id else {
        return;
    };
    let state = json!({
        "type": "state",
        "state": "ready",
        "accountEmail": email,
        "at": now_ms(),
    })
    .to_string();
    let reporter = app_reporter(&instance_id);
    let Ok(mut reporter) = reporter.lock() else {
        return;
    };
    reporter.latest_state = Some(state.clone());
    let send_failed = reporter
        .sender
        .as_ref()
        .map(|sender| sender.send(state).is_err())
        .unwrap_or(false);
    if send_failed {
        reporter.sender = None;
    }
}

/// Tell the launcher that the Chromium window owned by this app was closed.
/// The launcher removes the app record, then this process shuts down.
pub async fn report_browser_closed() {
    let instance_id = std::env::var("NOVAMAIL_INSTANCE_ID")
        .ok()
        .filter(|id| !id.is_empty())
        .or_else(|| {
            std::env::args().find_map(|argument| {
                argument
                    .strip_prefix("--novamail-instance=")
                    .map(str::to_string)
            })
        });
    let Some(instance_id) = instance_id else {
        return;
    };
    let message = json!({ "type": "browser-closed", "at": now_ms() }).to_string();
    let reporter = app_reporter(&instance_id);
    if let Ok(mut guard) = reporter.lock() {
        guard.latest_state = Some(message.clone());
    } else {
        return;
    }

    // The app usually has a live control connection already. If startup raced
    // the browser launch, give the reconnecting client a short chance to report
    // the close so the launcher can remove its persisted record.
    for _ in 0..25 {
        let sent = if let Ok(mut guard) = reporter.lock() {
            let result = guard.sender.as_ref().map(|sender| sender.send(message.clone()));
            match result {
                Some(Ok(())) => true,
                Some(Err(_)) => {
                    guard.sender = None;
                    false
                }
                None => false,
            }
        } else {
            false
        };
        if sent {
            tokio::time::sleep(Duration::from_millis(150)).await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn endpoint_path() -> PathBuf {
    store_dir().join("launcher-control.json")
}

/// Bind the launcher's control server. The port is ephemeral and the token is
/// fresh on every launch, so only processes the launcher itself started (and
/// handed the token to) can connect.
pub fn start_server(state: LauncherState) -> Result<(u16, String), String> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(|error| error.to_string())?;
    let port = listener
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();
    listener
        .set_nonblocking(true)
        .map_err(|error| error.to_string())?;

    let token = uuid::Uuid::new_v4().to_string();
    let token_for_task = token.clone();

    persist_endpoint(port, &token)?;

    tauri::async_runtime::spawn(async move {
        let Ok(listener) = tokio::net::TcpListener::from_std(listener) else {
            return;
        };
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let state = state.clone();
                    let token = token_for_task.clone();
                    tauri::async_runtime::spawn(async move {
                        serve_connection(state, token, stream).await;
                    });
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    });

    Ok((port, token))
}

fn persist_endpoint(port: u16, token: &str) -> Result<(), String> {
    let path = endpoint_path();
    let parent = path
        .parent()
        .ok_or_else(|| "The launcher control endpoint has no parent directory.".to_string())?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temp = path.with_file_name(format!(
        "launcher-control-{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    let content = serde_json::to_vec(&json!({
        "port": port,
        "token": token,
        "launcherPid": std::process::id(),
    }))
    .map_err(|error| error.to_string())?;
    let result = (|| {
        let mut file = fs::File::create(&temp).map_err(|error| error.to_string())?;
        file.write_all(&content)
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        fs::rename(&temp, &path).map_err(|error| error.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

fn read_endpoint() -> Option<Endpoint> {
    let value: Value = serde_json::from_str(&fs::read_to_string(endpoint_path()).ok()?).ok()?;
    let port = value.get("port")?.as_u64()?;
    let token = value.get("token")?.as_str()?.to_string();
    if port == 0 || port > u16::MAX as u64 || token.is_empty() {
        return None;
    }
    Some(Endpoint {
        port: port as u16,
        token,
    })
}

async fn serve_connection(state: LauncherState, token: String, stream: TcpStream) {
    let (read_half, write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    // The first line must be a valid handshake naming the instance.
    let Ok(Some(line)) = lines.next_line().await else {
        return;
    };
    let Ok(hello) = serde_json::from_str::<Value>(&line) else {
        return;
    };
    if hello.get("type").and_then(Value::as_str) != Some("hello")
        || hello.get("token").and_then(Value::as_str) != Some(token.as_str())
    {
        return;
    }
    let Some(id) = hello
        .get("instanceId")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return;
    };

    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<String>();
    launcher::register_sender(&state, &id, sender.clone());

    let writer = tauri::async_runtime::spawn(async move {
        let mut write_half = write_half;
        while let Some(line) = receiver.recv().await {
            if write_half.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });

    while let Ok(Some(line)) = lines.next_line().await {
        if let Ok(message) = serde_json::from_str::<Value>(&line) {
            launcher::handle_control_message(&state, &id, &message);
        }
    }

    launcher::unregister_sender(&state, &id, &sender);
    writer.abort();
}

/// Connect an app process back to its launcher and report readiness and identity.
pub fn start_client(app: AppHandle, port: u16, token: String, instance_id: String) {
    let reporter = app_reporter(&instance_id);
    tauri::async_runtime::spawn(async move {
        let initial = (port != 0 && !token.is_empty()).then_some(Endpoint { port, token });
        loop {
            let endpoint = read_endpoint().or_else(|| initial.clone());
            let Some(endpoint) = endpoint else {
                tokio::time::sleep(Duration::from_millis(750)).await;
                continue;
            };
            if let Err(error) =
                run_client_connection(&app, &endpoint, &instance_id, &reporter).await
            {
                if error == "shutdown" {
                    return;
                }
                log::debug!("launcher control channel disconnected: {error}");
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });
}

async fn run_client_connection(
    app: &AppHandle,
    endpoint: &Endpoint,
    instance_id: &str,
    reporter: &Arc<Mutex<AppReporter>>,
) -> Result<(), String> {
    let stream = tokio::time::timeout(
        Duration::from_secs(2),
        TcpStream::connect(("127.0.0.1", endpoint.port)),
    )
    .await
    .map_err(|_| "connection timed out".to_string())?
    .map_err(|error| error.to_string())?;
    let (read_half, mut write_half) = stream.into_split();

    let hello = json!({ "type": "hello", "token": endpoint.token, "instanceId": instance_id });
    let ready = json!({ "type": "state", "state": "ready", "at": now_ms() });
    let (outbound, mut outbound_rx) = unbounded_channel::<String>();
    let latest_state = if let Ok(mut reporter) = reporter.lock() {
        reporter.sender = Some(outbound.clone());
        reporter.latest_state.clone()
    } else {
        None
    };
    write_half
        .write_all(
            format!(
                "{hello}\n{ready}\n{}",
                latest_state
                    .map(|state| format!("{state}\n"))
                    .unwrap_or_default()
            )
            .as_bytes(),
        )
        .await
        .map_err(|error| error.to_string())?;

    let heartbeat_id = instance_id.to_string();
    let writer = tauri::async_runtime::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.tick().await;
        loop {
            let line = tokio::select! {
                line = outbound_rx.recv() => line,
                _ = interval.tick() => Some(json!({ "type": "heartbeat", "instanceId": heartbeat_id, "at": now_ms() }).to_string()),
            };
            let Some(line) = line else { break };
            if write_control_line(&mut write_half, &line).await.is_err() {
                break;
            }
        }
    });

    let mut lines = BufReader::new(read_half).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match message.get("type").and_then(Value::as_str) {
            Some("shutdown") => {
                writer.abort();
                clear_reporter_sender(reporter, &outbound);
                crate::runtime::shutdown_app(app).await;
                return Err("shutdown".to_string());
            }
            Some("focus") => focus_main(app),
            Some("rename") => {
                if let Some(name) = message.get("name").and_then(Value::as_str) {
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.set_title(name);
                    }
                }
            }
            _ => {}
        }
    }

    writer.abort();
    clear_reporter_sender(reporter, &outbound);
    Err("launcher connection closed".to_string())
}

async fn write_control_line(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    line: &str,
) -> Result<(), std::io::Error> {
    writer.write_all(format!("{line}\n").as_bytes()).await
}

fn clear_reporter_sender(reporter: &Arc<Mutex<AppReporter>>, sender: &UnboundedSender<String>) {
    if let Ok(mut reporter) = reporter.lock() {
        if reporter
            .sender
            .as_ref()
            .map(|current| current.same_channel(sender))
            .unwrap_or(false)
        {
            reporter.sender = None;
        }
    }
}

fn focus_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}
