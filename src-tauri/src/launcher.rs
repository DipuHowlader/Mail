//! App registry + supervisor — the Rust port of `desktop-runtime/launcher.cjs`.
//!
//! The launcher is deliberately thin: it owns the active app list, spawns one
//! independent process per app, and tracks each app's state over a small async
//! control channel. It never runs automation, telemetry or document rendering,
//! so a slow or broken app cannot block the launcher or any other app.
//!
//! Process model (unchanged from desktop runtime, minus Node IPC):
//!   - every app is the same executable with `--novamail-role=app` and a stable
//!     `--novamail-instance=app_…` id,
//!   - the child is *not* reaped when the launcher exits, so it survives a
//!     launcher crash (fault isolation),
//!   - the launcher still keeps the handle so it can stop an app on purpose.
//!
//! Control channel: desktop runtime used Node's IPC pipe (`stdio: 'ipc'`). There is no
//! equivalent in a Rust/Tauri shell, so we use a loopback TCP socket with a
//! per-launch random token. The protocol is newline-delimited JSON:
//!
//!   app → launcher   `{"type":"hello","token":…,"instanceId":…}`
//!                    `{"type":"state","state":"ready","at":…}`
//!                    `{"type":"heartbeat","at":…}`
//!   launcher → app   `{"type":"shutdown"}` / `{"type":"focus"}`
//!                    `{"type":"rename","name":…}`

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::store::{Store, now_ms, store_dir};

/// Shared, mutable supervisor state. Managed by Tauri so commands and the
/// control server can both reach it.
pub type LauncherState = Arc<Mutex<Registry>>;

/// Bounded per-app log ring, matching desktop runtime's `pushLog`.
const MAX_LOG_LINES: usize = 200;
const MAX_LOG_LINE_LEN: usize = 500;

// ---------------------------------------------------------------------------
// Durable app list
// ---------------------------------------------------------------------------

pub fn registry_path() -> std::path::PathBuf {
    if let Some(path) = std::env::var_os("NOVAMAIL_REGISTRY_PATH") {
        return std::path::PathBuf::from(path);
    }
    store_dir().join("instances.json")
}

#[derive(Clone, Debug)]
pub struct Record {
    pub id: String,
    pub name: String,
    pub account_email: String,
    pub created_at: i64,
    pub pid: Option<u32>,
    pub last_state: String,
    pub last_error: Option<String>,
}

fn is_record_id(id: &str) -> bool {
    let Some(rest) = id.strip_prefix("app_") else {
        return false;
    };
    let len = rest.chars().count();
    (4..=64).contains(&len)
        && rest
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
}

fn read_records() -> Vec<Record> {
    let Ok(text) = std::fs::read_to_string(registry_path()) else {
        return Vec::new();
    };
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    items
        .into_iter()
        .filter_map(|item| {
            let id = item.get("id").and_then(Value::as_str)?.to_string();
            if !is_record_id(&id) {
                return None;
            }
            Some(Record {
                id,
                name: item
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .unwrap_or("Novamail app")
                    .to_string(),
                account_email: item
                    .get("accountEmail")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                created_at: item
                    .get("createdAt")
                    .and_then(Value::as_i64)
                    .unwrap_or_else(now_ms),
                pid: item
                    .get("pid")
                    .and_then(Value::as_u64)
                    .map(|pid| pid as u32),
                last_state: item
                    .get("lastState")
                    .and_then(Value::as_str)
                    .unwrap_or("stopped")
                    .to_string(),
                last_error: item
                    .get("lastError")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        })
        .collect()
}

/// Successive default names: `Novamail app`, `Novamail app 2`, `Novamail app 3`…
///
/// This also fixes a latent bug in `desktop-runtime/launcher.cjs`, which called an
/// undefined `nextAppName()` when an app was created without a name.
fn next_app_name(records: &[Record]) -> String {
    let taken: std::collections::HashSet<&str> = records.iter().map(|r| r.name.as_str()).collect();
    if !taken.contains("Novamail app") {
        return "Novamail app".to_string();
    }
    let mut index = 2;
    loop {
        let candidate = format!("Novamail app {index}");
        if !taken.contains(candidate.as_str()) {
            return candidate;
        }
        index += 1;
    }
}

// ---------------------------------------------------------------------------
// Runtime state
// ---------------------------------------------------------------------------

struct ChildEntry {
    child: Child,
    state: String,
    started_at: i64,
    last_error: Option<String>,
    stopping: bool,
}

pub struct Registry {
    records: Vec<Record>,
    children: HashMap<String, ChildEntry>,
    logs: HashMap<String, Vec<String>>,
    control_port: Option<u16>,
    control_token: Option<String>,
    senders: HashMap<String, tokio::sync::mpsc::UnboundedSender<String>>,
    app: Option<AppHandle>,
    legacy_data_pending: bool,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            records: read_records(),
            children: HashMap::new(),
            logs: HashMap::new(),
            control_port: None,
            control_token: None,
            senders: HashMap::new(),
            app: None,
            legacy_data_pending: true,
        }
    }

    pub fn attach(&mut self, app: AppHandle) {
        self.app = Some(app.clone());
        let stale: Vec<(String, Option<u32>)> = self
            .records
            .iter()
            .filter(|record| !record.pid.map(is_process_alive).unwrap_or(false))
            .map(|record| (record.id.clone(), record.pid))
            .collect();
        for (id, pid) in stale {
            self.remove_record(&id);
            crate::runtime::cleanup_instance_data(&app, &id, pid);
        }
        let active_instances: HashSet<String> = self
            .records
            .iter()
            .filter(|record| record.pid.map(is_process_alive).unwrap_or(false))
            .map(|record| record.id.clone())
            .collect();
        let active_process_ids: HashSet<u32> = self
            .records
            .iter()
            .filter_map(|record| record.pid.filter(|pid| is_process_alive(*pid)))
            .collect();
        crate::runtime::cleanup_stale_instance_data(
            &app,
            &active_instances,
            &active_process_ids,
        );
        if active_process_ids.is_empty() {
            self.legacy_data_pending = !clear_legacy_app_data(&app);
        }
        self.persist();
    }

    pub fn set_control(&mut self, port: u16, token: String) {
        self.control_port = Some(port);
        self.control_token = Some(token);
    }

    fn record(&self, id: &str) -> Option<&Record> {
        self.records.iter().find(|record| record.id == id)
    }

    fn record_mut(&mut self, id: &str) -> Option<&mut Record> {
        self.records.iter_mut().find(|record| record.id == id)
    }

/// Persist active instance metadata so a still-running app can reconnect after
/// a launcher restart. Entries are deleted after the app process exits.
    pub fn persist(&self) {
        let path = registry_path();
        if self.records.is_empty() {
            let _ = std::fs::remove_file(path);
            return;
        }
        let durable: Vec<Value> = self
            .records
            .iter()
            .map(|record| {
                json!({
                    "id": record.id,
                    "name": record.name,
                    "accountEmail": record.account_email,
                    "createdAt": record.created_at,
                    "pid": record.pid,
                    "lastState": record.last_state,
                    "lastError": record.last_error,
                })
            })
            .collect();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(text) = serde_json::to_string_pretty(&Value::Array(durable)) {
            let _ = std::fs::write(&path, text);
        }
    }

    pub fn snapshot(&self) -> Vec<Value> {
        self.records
            .iter()
            .map(|record| {
                let live = self.children.get(&record.id);
                let connected = self.senders.contains_key(&record.id);
                let state = if let Some(live) = live {
                    live.state.clone()
                } else if connected {
                    record.last_state.clone()
                } else if record.pid.map(is_process_alive).unwrap_or(false) {
                    "orphaned".to_string()
                } else {
                    "stopped".to_string()
                };
                let pid = match live {
                    Some(live) => Some(live.child.id()),
                    None if connected || state == "orphaned" => record.pid,
                    None => None,
                };
                json!({
                    "id": record.id,
                    "name": record.name,
                    "accountEmail": record.account_email,
                    "createdAt": record.created_at,
                    "state": state,
                    "pid": pid,
                    "lastError": live
                        .and_then(|entry| entry.last_error.clone())
                        .or_else(|| record.last_error.clone()),
                    "startedAt": live.map(|entry| entry.started_at),
                })
            })
            .collect()
    }

    /// Apply a `state` message from an app's control channel.
    fn apply_state(
        &mut self,
        id: &str,
        state: &str,
        account_email: Option<&str>,
        last_error: Option<&str>,
    ) {
        if let Some(entry) = self.children.get_mut(id) {
            entry.state = state.to_string();
            entry.last_error = last_error.map(str::to_string);
        }
        if let Some(record) = self.record_mut(id) {
            record.last_state = state.to_string();
            if let Some(email) = account_email.filter(|email| !email.is_empty()) {
                record.account_email = email.to_string();
            }
            if last_error.is_some() {
                record.last_error = last_error.map(str::to_string);
            }
        }
        self.persist();
    }

    fn push_log(&mut self, id: &str, line: String) {
        if self.record(id).is_none() {
            return;
        }
        let buffer = self.logs.entry(id.to_string()).or_default();
        buffer.push(line);
        while buffer.len() > MAX_LOG_LINES {
            buffer.remove(0);
        }
    }

    fn remove_record(&mut self, id: &str) {
        self.records.retain(|record| record.id != id);
        self.logs.remove(id);
        self.senders.remove(id);
        self.records.shrink_to_fit();
        self.children.shrink_to_fit();
        self.logs.shrink_to_fit();
        self.senders.shrink_to_fit();
    }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

pub fn broadcast(state: &LauncherState) {
    let (app, snapshot) = locked(state, |registry| {
        (registry.app.clone(), registry.snapshot())
    });
    if let Some(app) = app {
        let _ = app.emit("launcher:changed", snapshot);
    }
}

fn emit_log(state: &LauncherState, id: &str, lines: &[String]) {
    let app = locked(state, |registry| registry.app.clone());
    if let Some(app) = app {
        let _ = app.emit("launcher:log", json!({ "id": id, "lines": lines }));
    }
}

/// Run `f` while holding the registry lock, recovering from a poisoned mutex so
/// a panic in one command cannot take the whole supervisor down.
fn locked<T>(state: &LauncherState, f: impl FnOnce(&mut Registry) -> T) -> T {
    match state.lock() {
        Ok(mut guard) => f(&mut guard),
        Err(poisoned) => f(&mut poisoned.into_inner()),
    }
}

fn clear_legacy_app_data(app: &AppHandle) -> bool {
    let Some(store) = app.try_state::<Mutex<Store>>() else {
        return false;
    };
    let Ok(mut store) = store.lock() else {
        return false;
    };
    match store.clear_legacy_app_data() {
        Ok(()) => true,
        Err(error) => {
            log::warn!("Could not clear legacy app data: {error}");
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Process liveness / termination
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn is_process_alive(pid: u32) -> bool {
    use std::ffi::c_void;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(
            dw_desired_access: u32,
            b_inherit_handle: i32,
            dw_process_id: u32,
        ) -> *mut c_void;
        fn GetExitCodeProcess(h_process: *mut c_void, lp_exit_code: *mut u32) -> i32;
        fn CloseHandle(h_object: *mut c_void) -> i32;
    }

    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const STILL_ACTIVE: u32 = 259;

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(handle, &mut code);
        CloseHandle(handle);
        ok != 0 && code == STILL_ACTIVE
    }
}

#[cfg(not(windows))]
fn is_process_alive(pid: u32) -> bool {
    // Portable-enough check via /proc; non-Windows is a secondary target for now.
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

/// On Windows a plain `kill()` misses the WebView2 children. `taskkill /T /F`
/// removes the whole tree so no orphaned browser is left behind.
pub fn hard_kill_pid(pid: u32) {
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    {
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
    }
}

// ---------------------------------------------------------------------------
// Spawning
// ---------------------------------------------------------------------------

fn spawn_log_reader<R: Read + Send + 'static>(state: LauncherState, id: String, reader: R) {
    std::thread::spawn(move || {
        let buffered = BufReader::new(reader);
        for line in buffered.lines() {
            let Ok(line) = line else { break };
            let line = line.trim_end();
            if line.is_empty() {
                continue;
            }
            let line = if line.len() > MAX_LOG_LINE_LEN {
                line[..MAX_LOG_LINE_LEN].to_string()
            } else {
                line.to_string()
            };
            locked(&state, |registry| registry.push_log(&id, line.clone()));
            emit_log(&state, &id, std::slice::from_ref(&line));
        }
    });
}

fn spawn_app_process(command: &mut Command) -> std::io::Result<Child> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        // CREATE_BREAKAWAY_FROM_JOB lets app windows survive if Windows or a
        // parent launcher was itself started inside a kill-on-close job.
        command.creation_flags(0x0100_0000);
        match command.spawn() {
            Ok(child) => Ok(child),
            Err(error) if matches!(error.raw_os_error(), Some(5 | 87)) => {
                // Some managed environments prohibit breakaway. Keep the
                // launcher usable there and report the ordinary spawn error if
                // the fallback also fails.
                command.creation_flags(0);
                let child = command.spawn()?;
                log::warn!(
                    "Windows denied CREATE_BREAKAWAY_FROM_JOB; app crash isolation may be limited in this environment."
                );
                Ok(child)
            }
            Err(error) => Err(error),
        }
    }
    #[cfg(not(windows))]
    {
        command.spawn()
    }
}

fn spawn_app(state: &LauncherState, id: &str) {
    let resolved = locked(state, |registry| {
        registry.record(id).map(|record| {
            (
                record.name.clone(),
                registry.control_port,
                registry.control_token.clone(),
            )
        })
    });
    let Some((name, port, token)) = resolved else {
        return;
    };

    let Ok(exe) = std::env::current_exe() else {
        return;
    };

    let mut command = Command::new(exe);
    command
        .arg("--novamail-role=app")
        .arg(format!("--novamail-instance={id}"))
        .arg(format!("--novamail-instance-name={name}"))
        .env("NOVAMAIL_ROLE", "app")
        .env("NOVAMAIL_INSTANCE_ID", id)
        .env("NOVAMAIL_INSTANCE_NAME", &name)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(port) = port {
        command.env("NOVAMAIL_CONTROL_PORT", port.to_string());
    }
    if let Some(token) = &token {
        command.env("NOVAMAIL_CONTROL_TOKEN", token);
    }
    // A child must never inherit the launcher's self-test mode or flag.
    command.env_remove("NOVAMAIL_SELFTEST_MODE");
    command.env_remove("NOVAMAIL_SELFTEST");

    let mut child = match spawn_app_process(&mut command) {
        Ok(child) => child,
        Err(error) => {
            locked(state, |registry| {
                if let Some(record) = registry.record_mut(id) {
                    record.last_state = "failed".to_string();
                    record.last_error = Some(format!("The app could not be started: {error}"));
                }
                registry.persist();
            });
            broadcast(state);
            return;
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let pid = child.id();

    locked(state, |registry| {
        if let Some(record) = registry.record_mut(id) {
            record.pid = Some(pid);
            record.last_state = "starting".to_string();
            record.last_error = None;
        }
        registry.children.insert(
            id.to_string(),
            ChildEntry {
                child,
                state: "starting".to_string(),
                started_at: now_ms(),
                last_error: None,
                stopping: false,
            },
        );
        registry.persist();
    });

    if let Some(stdout) = stdout {
        spawn_log_reader(state.clone(), id.to_string(), stdout);
    }
    if let Some(stderr) = stderr {
        spawn_log_reader(state.clone(), id.to_string(), stderr);
    }

    broadcast(state);
}

/// Poll live children so exits are noticed without blocking a command.
pub fn start_reaper(state: LauncherState) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_millis(500));

            let (changed, cleanup, app, active_instances, active_process_ids, clear_legacy) =
                locked(&state, |registry| {
                let ids: Vec<String> = registry.children.keys().cloned().collect();
                let mut changed = false;
                let mut cleanup = Vec::new();

                for id in ids {
                    let outcome = match registry.children.get_mut(&id) {
                        Some(entry) => match entry.child.try_wait() {
                            Ok(Some(_status)) => Some((entry.stopping, entry.child.id())),
                            Ok(None) => None,
                            Err(_) => Some((entry.stopping, entry.child.id())),
                        },
                        None => None,
                    };

                    let Some((stopping, process_id)) = outcome else {
                        continue;
                    };
                    registry.children.remove(&id);
                    changed = true;
                    cleanup.push((id.clone(), Some(process_id)));

                    if stopping {
                        if let Some(record) = registry.record_mut(&id) {
                            record.pid = None;
                            record.last_state = "stopped".to_string();
                            record.account_email.clear();
                            record.last_error = None;
                        }
                    } else if let Some(record) = registry.record_mut(&id) {
                        // Keep crashed app records so the launcher can restart
                        // the same instance and preserve its name/account.
                        record.pid = None;
                        record.last_state = "failed".to_string();
                        record.account_email.clear();
                        record.last_error =
                            Some("The app process exited unexpectedly.".to_string());
                    }
                }

                // Apps adopted from an earlier launcher process have no Child
                // handle. Reap them by PID too so their rows and instance data
                // disappear as soon as those processes exit.
                let orphaned: Vec<(String, u32, String)> = registry
                    .records
                    .iter()
                    .filter(|record| !registry.children.contains_key(&record.id))
                    .filter_map(|record| {
                        let process_id = record.pid?;
                        (!is_process_alive(process_id)).then(|| {
                            (record.id.clone(), process_id, record.last_state.clone())
                        })
                    })
                    .collect();
                for (id, process_id, last_state) in orphaned {
                    cleanup.push((id.clone(), Some(process_id)));
                    changed = true;
                    if last_state == "stopping" {
                        if let Some(record) = registry.record_mut(&id) {
                            record.pid = None;
                            record.account_email.clear();
                            record.last_state = "stopped".to_string();
                            record.last_error = None;
                        }
                    } else if let Some(record) = registry.record_mut(&id) {
                        // Retain adopted app records after a crash so they can
                        // be restarted from the launcher.
                        record.pid = None;
                        record.account_email.clear();
                        record.last_state = "failed".to_string();
                        record.last_error =
                            Some("The app process exited unexpectedly.".to_string());
                    }
                }

                let active_instances: HashSet<String> = registry
                    .records
                    .iter()
                    .filter(|record| record.pid.map(is_process_alive).unwrap_or(false))
                    .map(|record| record.id.clone())
                    .collect();
                let active_process_ids: HashSet<u32> = registry
                    .records
                    .iter()
                    .filter_map(|record| record.pid.filter(|pid| is_process_alive(*pid)))
                    .collect();
                let clear_legacy = registry.legacy_data_pending && active_process_ids.is_empty();

                if changed {
                    registry.children.shrink_to_fit();
                    registry.persist();
                }
                (
                    changed,
                    cleanup,
                    registry.app.clone(),
                    active_instances,
                    active_process_ids,
                    clear_legacy,
                )
            });

            if let Some(app) = app {
                for (id, process_id) in cleanup {
                    crate::runtime::cleanup_instance_data(&app, &id, process_id);
                }
                if changed || clear_legacy {
                    crate::runtime::cleanup_stale_instance_data(
                        &app,
                        &active_instances,
                        &active_process_ids,
                    );
                }
                if clear_legacy {
                    if clear_legacy_app_data(&app) {
                        locked(&state, |registry| registry.legacy_data_pending = false);
                    }
                }
            }

            if changed {
                broadcast(&state);
            }
        }
    });
}

fn send_command(state: &LauncherState, id: &str, command: Value) -> bool {
    let Some(sender) = locked(state, |registry| registry.senders.get(id).cloned()) else {
        return false;
    };
    sender.send(format!("{command}\n")).is_ok()
}

// ---------------------------------------------------------------------------
// Lifecycle operations
// ---------------------------------------------------------------------------

pub fn create_app(state: &LauncherState, name: Option<String>) -> Option<Value> {
    let id = locked(state, |registry| {
        let trimmed = name
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty());
        let id = format!("app_{}", uuid::Uuid::new_v4().simple());
        let name = trimmed
            .map(|name| name.chars().take(60).collect::<String>())
            .unwrap_or_else(|| next_app_name(&registry.records));
        let record = Record {
            id: id.clone(),
            name,
            account_email: String::new(),
            created_at: now_ms(),
            pid: None,
            last_state: "stopped".to_string(),
            last_error: None,
        };
        registry.records.push(record);
        registry.persist();
        id
    });

    spawn_app(state, &id);

    locked(state, |registry| {
        registry
            .snapshot()
            .into_iter()
            .find(|item| item.get("id").and_then(Value::as_str) == Some(id.as_str()))
    })
}

pub fn open_app(state: &LauncherState, id: &str) {
    let (managed, connected, pid) = locked(state, |registry| {
        (
            registry.children.contains_key(id),
            registry.senders.contains_key(id),
            registry.record(id).and_then(|record| record.pid),
        )
    });
    if managed || connected {
        let _ = send_command(state, id, json!({ "type": "focus" }));
        return;
    }

    // A process from an earlier launcher session may still be reconnecting.
    // Keep it alive; its client retries until it reaches this launcher's new
    // endpoint. Starting another copy here would split the app's profile/state.
    if pid.map(is_process_alive).unwrap_or(false) {
        return;
    }

    locked(state, |registry| {
        if let Some(record) = registry.record_mut(id) {
            record.pid = None;
        }
    });
    spawn_app(state, id);
}

pub async fn stop_app(state: &LauncherState, id: &str, force: bool) {
    let (has_child, has_sender, pid) = locked(state, |registry| {
        if let Some(entry) = registry.children.get_mut(id) {
            entry.stopping = true;
            entry.state = "stopping".to_string();
        }
        if let Some(record) = registry.record_mut(id) {
            record.last_state = "stopping".to_string();
        }
        registry.persist();
        (
            registry.children.contains_key(id),
            registry.senders.contains_key(id),
            registry.record(id).and_then(|record| record.pid),
        )
    });

    broadcast(state);

    if !has_child && has_sender {
        // The child is adopted over the reconnecting control socket, so this
        // launcher has no std::process::Child handle. Ask it to exit cleanly,
        // then fall back to terminating its full process tree after the grace
        // period, just as we do for a directly spawned child.
        let _ = send_command(state, id, json!({ "type": "shutdown" }));
        if !force {
            for _ in 0..25 {
                if !pid.map(is_process_alive).unwrap_or(false) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
        if let Some(pid) = pid.filter(|pid| is_process_alive(*pid)) {
            hard_kill_pid(pid);
            wait_for_process_exit(pid).await;
        }
        locked(state, |registry| {
            if let Some(record) = registry.record_mut(id) {
                record.pid = None;
                record.last_state = "stopped".to_string();
                record.last_error = None;
            }
            registry.persist();
        });
        broadcast(state);
        return;
    }

    if !has_child {
        // A process with no live control channel cannot be stopped gracefully.
        if let Some(pid) = pid.filter(|pid| is_process_alive(*pid)) {
            hard_kill_pid(pid);
            wait_for_process_exit(pid).await;
            locked(state, |registry| {
                if let Some(record) = registry.record_mut(id) {
                    record.pid = None;
                    record.last_state = "stopped".to_string();
                    record.last_error = None;
                }
                registry.persist();
            });
            broadcast(state);
        }
        return;
    }

    let _ = send_command(state, id, json!({ "type": "shutdown" }));
    if !force {
        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
    }

    // If it is still alive after the grace period, take the whole tree down.
    let settled = locked(state, |registry| {
        let status = match registry.children.get_mut(id) {
            Some(entry) => entry.child.try_wait(),
            None => return false,
        };
        match status {
            Ok(None) => {
                if let Some(mut entry) = registry.children.remove(id) {
                    hard_kill_pid(entry.child.id());
                    let _ = entry.child.wait();
                }
            }
            _ => {
                registry.children.remove(id);
            }
        }
        if let Some(record) = registry.record_mut(id) {
            record.pid = None;
            record.last_state = "stopped".to_string();
            record.last_error = None;
        }
        registry.persist();
        true
    });

    if settled {
        broadcast(state);
    }
}

async fn wait_for_process_exit(process_id: u32) {
    for _ in 0..30 {
        if !is_process_alive(process_id) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

// ---------------------------------------------------------------------------
// Tauri commands (mirror the launcher:* channels)
// ---------------------------------------------------------------------------

fn cleanup_instance(state: &LauncherState, id: &str, process_id: Option<u32>) {
    let app = locked(state, |registry| registry.app.clone());
    if let Some(app) = app {
        crate::runtime::cleanup_instance_data(&app, id, process_id);
    }
}

fn remove_app_record(state: &LauncherState, id: &str, process_id: Option<u32>) {
    locked(state, |registry| {
        registry.remove_record(id);
        registry.persist();
    });
    cleanup_instance(state, id, process_id);
    broadcast(state);
}

#[tauri::command]
pub fn launcher_list(state: State<'_, LauncherState>) -> Vec<Value> {
    locked(state.inner(), |registry| registry.snapshot())
}

#[tauri::command]
pub fn launcher_create(options: Option<Value>, state: State<'_, LauncherState>) -> Option<Value> {
    let name = options
        .as_ref()
        .and_then(|options| options.get("name"))
        .and_then(Value::as_str)
        .map(str::to_string);
    create_app(state.inner(), name)
}

#[tauri::command]
pub fn launcher_open(id: String, state: State<'_, LauncherState>) -> Option<Value> {
    let state = state.inner();
    open_app(state, &id);
    locked(state, |registry| {
        registry
            .snapshot()
            .into_iter()
            .find(|item| item.get("id").and_then(Value::as_str) == Some(id.as_str()))
    })
}

#[tauri::command]
pub async fn launcher_close(
    id: String,
    state: State<'_, LauncherState>,
) -> Result<Vec<Value>, String> {
    let state = state.inner().clone();
    let process_id = locked(&state, |registry| {
        registry.record(&id).and_then(|record| record.pid)
    });
    stop_app(&state, &id, false).await;
    remove_app_record(&state, &id, process_id);
    Ok(locked(&state, |registry| registry.snapshot()))
}

#[tauri::command]
pub async fn launcher_restart(
    id: String,
    state: State<'_, LauncherState>,
) -> Result<Option<Value>, String> {
    let state = state.inner().clone();
    let process_id = locked(&state, |registry| {
        registry.record(&id).and_then(|record| record.pid)
    });
    stop_app(&state, &id, false).await;
    cleanup_instance(&state, &id, process_id);
    locked(&state, |registry| {
        registry.logs.remove(&id);
        registry.logs.shrink_to_fit();
        if let Some(record) = registry.record_mut(&id) {
            record.account_email.clear();
            record.last_error = None;
        }
    });
    open_app(&state, &id);
    Ok(locked(&state, |registry| {
        registry
            .snapshot()
            .into_iter()
            .find(|item| item.get("id").and_then(Value::as_str) == Some(id.as_str()))
    }))
}

#[tauri::command]
pub async fn launcher_remove(
    id: String,
    state: State<'_, LauncherState>,
) -> Result<Vec<Value>, String> {
    let state = state.inner().clone();
    let process_id = locked(&state, |registry| {
        registry.record(&id).and_then(|record| record.pid)
    });
    // Let the app close its automation service and remove its throwaway
    // Chromium profile before falling back to process-tree termination.
    stop_app(&state, &id, false).await;
    remove_app_record(&state, &id, process_id);
    Ok(locked(&state, |registry| registry.snapshot()))
}

#[tauri::command]
pub fn launcher_rename(id: String, name: String, state: State<'_, LauncherState>) -> Vec<Value> {
    let state = state.inner();
    let trimmed = name.trim().chars().take(60).collect::<String>();
    if !trimmed.is_empty() {
        locked(state, |registry| {
            if let Some(record) = registry.record_mut(&id) {
                record.name = trimmed.clone();
            }
            registry.persist();
        });
        let _ = send_command(state, &id, json!({ "type": "rename", "name": trimmed }));
    }
    broadcast(state);
    locked(state, |registry| registry.snapshot())
}

#[tauri::command]
pub fn launcher_focus(id: String, state: State<'_, LauncherState>) -> Vec<Value> {
    let state = state.inner();
    let _ = send_command(state, &id, json!({ "type": "focus" }));
    locked(state, |registry| registry.snapshot())
}

#[tauri::command]
pub fn launcher_logs(id: String, state: State<'_, LauncherState>) -> Vec<String> {
    logs_of(state.inner(), &id)
}

/// Read one app's current snapshot row (used by commands and self-tests).
pub fn snapshot_of(state: &LauncherState, id: &str) -> Option<Value> {
    locked(state, |registry| {
        registry
            .snapshot()
            .into_iter()
            .find(|item| item.get("id").and_then(Value::as_str) == Some(id))
    })
}

/// Read one app's bounded log buffer.
pub fn logs_of(state: &LauncherState, id: &str) -> Vec<String> {
    locked(state, |registry| {
        registry.logs.get(id).cloned().unwrap_or_default()
    })
}

#[tauri::command]
pub fn launcher_info() -> Value {
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "appRoot": std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|parent| parent.display().to_string()))
            .unwrap_or_default(),
        "canLaunch": true,
    })
}

// ---------------------------------------------------------------------------
// Control-channel hooks (called by `control.rs`)
// ---------------------------------------------------------------------------

pub fn register_sender(
    state: &LauncherState,
    id: &str,
    sender: tokio::sync::mpsc::UnboundedSender<String>,
) {
    locked(state, |registry| {
        registry.senders.insert(id.to_string(), sender);
    });
}

pub fn unregister_sender(
    state: &LauncherState,
    id: &str,
    sender: &tokio::sync::mpsc::UnboundedSender<String>,
) {
    let removed = locked(state, |registry| {
        if registry
            .senders
            .get(id)
            .map(|current| current.same_channel(sender))
            .unwrap_or(false)
        {
            registry.senders.remove(id);
            true
        } else {
            false
        }
    });
    if removed {
        broadcast(state);
    }
}

/// Apply a message received on an app's control channel.
pub fn handle_control_message(state: &LauncherState, id: &str, message: &Value) {
    match message.get("type").and_then(Value::as_str) {
        Some("browser-closed") => {
            locked(state, |registry| {
                if let Some(record) = registry.record_mut(id) {
                    record.last_state = "closing".to_string();
                    record.account_email.clear();
                    registry.persist();
                }
            });
            broadcast(state);
        }
        Some("state") => {
            let state_name = message
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("ready");
            let account_email = message.get("accountEmail").and_then(Value::as_str);
            let last_error = message.get("lastError").and_then(Value::as_str);
            locked(state, |registry| {
                registry.apply_state(id, state_name, account_email, last_error)
            });
            broadcast(state);
        }
        // Heartbeats are informational for now; liveness comes from the process.
        _ => {}
    }
}
