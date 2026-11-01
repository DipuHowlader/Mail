//! Automation orchestration (Phase 4).
//!
//! Ports the queue/worker half of `desktop-runtime/automation.cjs` to Rust on top of
//! the [`crate::browser`] driver: the profile/connection lifecycle, the
//! per-instance run registry, pause/resume/stop/retry, and the
//! `automation:telemetry` event contract the React frontend already consumes.
//!
//! The actual Gmail interaction (`sendOneCore` in desktop runtime) is behind
//! [`mail_sender::MailSender`]; see that module for why the split exists. The
//! command surface and observable behaviour are a one-for-one port so the
//! renderer does not change:
//!
//! | desktop runtime channel                | Tauri command                     |
//! |---------------------------------|-----------------------------------|
//! | `automation:addProfile`         | `automation_add_profile`          |
//! | `automation:listProfiles`       | `automation_list_profiles`        |
//! | `automation:listRuns`           | `automation_list_runs`            |
//! | `automation:start`              | `automation_start`                |
//! | `automation:retry`              | `automation_retry`                |
//! | `automation:stop|pause|resume`  | `automation_stop|pause|resume`    |
//! | `automation:closeProfile`       | `automation_close_profile`        |
//! | `automation:transmissionState`  | `automation_transmission_state`   |
//! | `automation:resetAllInstances`  | `automation_reset_all_instances`  |

#[cfg(windows)]
mod clipboard;
pub mod gmail;
pub mod mail_sender;
pub mod selftest;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Map, Value};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::browser::chromium::ChromiumDriver;
use crate::browser::{paths, BrowserDriver, DriverConfig, PageId};
use crate::store::{now_ms, Store};

use mail_sender::{MailSender, SendFailure, SendRequest};

/// The one browser-instance cap desktop runtime enforced per run.
const MAX_RECIPIENTS_PER_INSTANCE: u64 = 500;
/// How many finished runs stay addressable for `automation:retry`.
const COMPLETED_RUN_HISTORY: usize = 50;
/// Bound the complete browser interaction for one recipient so a stalled
/// renderer/CDP command cannot leave the instance registered as sending forever.
const SEND_OPERATION_TIMEOUT: Duration = Duration::from_secs(180);

// --- State ---------------------------------------------------------------------

/// How far a profile's Gmail login has got.
#[derive(Clone, Debug)]
enum LoginState {
    Pending,
    Ready,
    Failed(String),
}

/// A launched browser instance (desktop runtime's `accounts` entry).
struct Account {
    id: String,
    email: Option<String>,
    name: String,
    ready: bool,
    expected_email: String,
    driver: Arc<dyn BrowserDriver>,
    page: PageId,
    tearing_down: bool,
    login: LoginState,
}

/// One instance's send queue and its progress (desktop runtime's worker object).
struct Worker {
    session_id: String,
    campaign_id: String,
    instance_id: String,
    worker_id: String,
    slot_id: String,
    account_email: String,
    process_id: Option<i64>,
    queue: Vec<Value>,
    retry_queue: Vec<Value>,
    current_index: usize,
    current_recipient: Option<String>,
    sent_count: i64,
    failed_count: i64,
    total: i64,
    status: String,
    paused: bool,
    aborted: bool,
    /// A pause landed while this worker was mid-send: Gmail's compose still
    /// holds the abandoned draft and must be closed once before the queue
    /// composes into the same window again.
    compose_dirty: bool,
    error_message: Option<String>,
    updated_at: i64,
}

/// A started run: its payload and every worker it spawned.
struct RunRecord {
    session_id: String,
    campaign_id: String,
    payload: Mutex<Value>,
    workers: Mutex<Vec<Arc<Mutex<Worker>>>>,
}

#[derive(Default)]
struct AutomationInner {
    accounts: HashMap<String, Account>,
    active_workers: HashMap<String, Arc<Mutex<Worker>>>,
    runs: HashMap<String, Arc<RunRecord>>,
    active_sessions: HashSet<String>,
    run_order: VecDeque<String>,
    retry_locks: HashSet<String>,
    reset_in_progress: bool,
}

/// Test-only capture of emitted telemetry.
pub struct CollectorSink {
    events: Mutex<Vec<Value>>,
}

impl CollectorSink {
    pub fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
        }
    }

    fn push(&self, event: Value) {
        if let Ok(mut guard) = self.events.lock() {
            guard.push(event);
        }
    }

    pub fn snapshot(&self) -> Vec<Value> {
        self.events
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }
}

impl Default for CollectorSink {
    fn default() -> Self {
        Self::new()
    }
}

/// The state every command and worker task shares. Cheap to clone (four Arcs).
#[derive(Clone)]
struct Shared {
    state: Arc<Mutex<AutomationInner>>,
    sender: Arc<dyn MailSender>,
    app: Arc<Mutex<Option<AppHandle>>>,
    sink: Option<Arc<CollectorSink>>,
}

/// Managed automation state.
pub struct AutomationService {
    shared: Shared,
}

impl Default for AutomationService {
    fn default() -> Self {
        Self::new()
    }
}

impl AutomationService {
    pub fn new() -> Self {
        Self::build(
            Arc::new(mail_sender::ChromiumMailSender::new()),
            None,
        )
    }

    pub fn build(sender: Arc<dyn MailSender>, sink: Option<Arc<CollectorSink>>) -> Self {
        Self {
            shared: Shared {
                state: Arc::new(Mutex::new(AutomationInner::default())),
                sender,
                app: Arc::new(Mutex::new(None)),
                sink,
            },
        }
    }

    fn shared(&self) -> Shared {
        self.shared.clone()
    }

    /// Remember the app handle so background tasks (login detection) can emit
    /// telemetry. Called once from `setup`; idempotent.
    pub fn attach_app(&self, app: &AppHandle) {
        if let Ok(mut guard) = self.shared.app.lock() {
            if guard.is_none() {
                *guard = Some(app.clone());
            }
        }
    }

    /// Close every headed browser before an app process exits.
    pub async fn shutdown_all(&self) {
        let shared = self.shared();
        let ids: Vec<String> = shared
            .state
            .lock()
            .map(|state| state.accounts.keys().cloned().collect())
            .unwrap_or_default();
        for id in ids {
            close_browser(&shared, &id, "browser-closed").await;
        }
    }
}

// --- Telemetry -----------------------------------------------------------------

fn emit(shared: &Shared, event: Value) {
    if let Some(sink) = &shared.sink {
        sink.push(event.clone());
    }
    if let Ok(guard) = shared.app.lock() {
        if let Some(app) = guard.as_ref() {
            let _ = app.emit("automation:telemetry", event);
        }
    }
}

fn logs_to_string(value: &Value) -> String {
    match value {
        Value::Array(items) => items
            .iter()
            .map(|item| match item {
                Value::String(text) => text.clone(),
                Value::Null => String::new(),
                other => other.to_string(),
            })
            .collect::<Vec<_>>()
            .join(" "),
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn obj_str(map: &Map<String, Value>, key: &str) -> Option<String> {
    map.get(key).and_then(Value::as_str).map(str::to_string)
}

fn obj_i64(map: &Map<String, Value>, key: &str) -> Option<i64> {
    map.get(key).and_then(Value::as_i64)
}

/// Normalize a partial telemetry payload exactly the way `sendTelemetry` did:
/// fill the identity/count fields, coerce `logs` to a string, and drop events
/// that carry no instance identity.
fn send_telemetry(shared: &Shared, payload: Value) {
    let Value::Object(mut object) = payload else {
        return;
    };
    let instance_id = object
        .get("instanceId")
        .and_then(Value::as_str)
        .or_else(|| object.get("accountId").and_then(Value::as_str))
        .unwrap_or("")
        .to_string();
    if instance_id.is_empty() {
        log::warn!("[automation] Dropping telemetry without an instance identity.");
        return;
    }

    let logs = object.get("logs").map(logs_to_string).unwrap_or_default();
    let total = obj_i64(&object, "total").unwrap_or(0);
    let current_index = obj_i64(&object, "currentIndex").unwrap_or(0);
    let recipient = object
        .get("recipient")
        .and_then(Value::as_str)
        .or_else(|| object.get("activeRecipient").and_then(Value::as_str))
        .map(str::to_string);
    let status = obj_str(&object, "status").unwrap_or_else(|| "unknown".to_string());
    let event = obj_str(&object, "event").unwrap_or_else(|| status.clone());
    let sent_count = obj_i64(&object, "sentCount").unwrap_or(0);
    let failed_count = obj_i64(&object, "failedCount").unwrap_or(0);
    let timestamp = obj_i64(&object, "timestamp").unwrap_or_else(now_ms);
    let remaining = obj_i64(&object, "remaining").unwrap_or_else(|| (total - sent_count).max(0));
    let session_id = obj_str(&object, "sessionId").unwrap_or_default();
    let account_id = obj_str(&object, "accountId").unwrap_or_else(|| instance_id.clone());
    let worker_id = obj_str(&object, "workerId").unwrap_or_default();
    let account_email = obj_str(&object, "accountEmail").unwrap_or_default();
    let error = obj_str(&object, "error").unwrap_or_default();
    let process_id = object
        .get("processId")
        .filter(|value| value.is_i64() || value.is_u64())
        .cloned()
        .unwrap_or(Value::Null);

    object.insert("timestamp".into(), json!(timestamp));
    object.insert("status".into(), json!(status));
    object.insert("event".into(), json!(event));
    object.insert("sessionId".into(), json!(session_id));
    object.insert("instanceId".into(), json!(instance_id));
    object.insert("accountId".into(), json!(account_id));
    object.insert("workerId".into(), json!(worker_id));
    object.insert("processId".into(), process_id);
    object.insert("accountEmail".into(), json!(account_email));
    object.insert(
        "recipient".into(),
        recipient.clone().map(Value::String).unwrap_or(Value::Null),
    );
    object.insert(
        "activeRecipient".into(),
        recipient.map(Value::String).unwrap_or(Value::Null),
    );
    object.insert("sentCount".into(), json!(sent_count));
    object.insert("failedCount".into(), json!(failed_count));
    object.insert("currentIndex".into(), json!(current_index));
    object.insert("total".into(), json!(total));
    object.insert("remaining".into(), json!(remaining));
    object.insert("error".into(), json!(error));
    object.insert("logs".into(), json!(logs));

    emit(shared, Value::Object(object));
}

/// `compactRetryRecipients`: strip the heavy/normalized fields before a retry
/// queue is put on the wire.
fn compact_retry(queue: &[Value]) -> Value {
    const STRIP: [&str; 7] = [
        "html",
        "text",
        "subject",
        "attachments",
        "accountId",
        "instanceId",
        "slotId",
    ];
    Value::Array(
        queue
            .iter()
            .map(|recipient| {
                let mut object = recipient.as_object().cloned().unwrap_or_default();
                for key in STRIP {
                    object.remove(key);
                }
                Value::Object(object)
            })
            .collect(),
    )
}

/// `emitWorkerTelemetry`: fill the worker/run identity and counts, then hand off
/// to `send_telemetry`.
fn emit_worker(
    shared: &Shared,
    session_id: &str,
    campaign_id: &str,
    worker: &Arc<Mutex<Worker>>,
    status: &str,
    mut payload: Map<String, Value>,
) {
    let mut guard = match worker.lock() {
        Ok(guard) => guard,
        Err(_) => return,
    };
    let recipient = payload
        .get("recipient")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| guard.current_recipient.clone());
    let remaining = payload
        .get("remaining")
        .and_then(Value::as_i64)
        .unwrap_or_else(|| (guard.total - guard.sent_count).max(0));
    let current_index = payload
        .get("currentIndex")
        .and_then(Value::as_i64)
        .unwrap_or(guard.current_index as i64);
    let retry = compact_retry(&guard.retry_queue);
    let recipient_id = payload
        .get("recipientId")
        .cloned()
        .filter(|value| !value.is_null())
        .unwrap_or_else(|| {
            let matches = recipient
                .as_deref()
                .map(|current| {
                    guard
                        .queue
                        .get(guard.current_index)
                        .and_then(|queued| queued.get("email"))
                        .and_then(Value::as_str)
                        .map(|queued| queued.eq_ignore_ascii_case(current))
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            if matches {
                guard
                    .queue
                    .get(guard.current_index)
                    .and_then(|queued| queued.get("id"))
                    .cloned()
                    .unwrap_or(Value::Null)
            } else {
                Value::Null
            }
        });

    let now = now_ms();
    guard.updated_at = now;
    let instance_id = guard.instance_id.clone();
    let worker_id = guard.worker_id.clone();
    let slot_id = guard.slot_id.clone();
    let account_email = guard.account_email.clone();
    let process_id = guard.process_id;
    // Every telemetry event also carries the emails still queued (never sent
    // and never attempted) so a "copy not sent" action can list failures and
    // the untouched remainder together.
    let queue_tail: Vec<String> = {
        let failed_or_retried: HashSet<String> = guard
            .retry_queue
            .iter()
            .filter_map(|entry| entry.get("email").and_then(Value::as_str))
            .map(|email| email.to_ascii_lowercase())
            .collect();
        guard
            .queue
            .iter()
            .skip(guard.current_index)
            .filter_map(|entry| entry.get("email").and_then(Value::as_str))
            .filter(|email| !failed_or_retried.contains(&email.to_ascii_lowercase()))
            .map(|email| email.to_string())
            .collect()
    };
    let sent_count = guard.sent_count;
    let failed_count = guard.failed_count;
    let total = guard.total;
    drop(guard);

    payload.insert("retryRecipients".into(), retry);
    payload.insert("queueTail".into(), json!(queue_tail));
    payload.insert("recipientId".into(), recipient_id);
    payload.insert("status".into(), json!(status));
    payload.insert("event".into(), json!(status));
    payload.insert("instanceId".into(), json!(instance_id));
    payload.insert("accountId".into(), json!(instance_id));
    payload.insert("workerId".into(), json!(worker_id));
    payload.insert("sessionId".into(), json!(session_id));
    payload.insert("campaignId".into(), json!(campaign_id));
    payload.insert("slotId".into(), json!(slot_id));
    payload.insert(
        "processId".into(),
        process_id.map(|pid| json!(pid)).unwrap_or(Value::Null),
    );
    payload.insert("accountEmail".into(), json!(account_email));
    payload.insert(
        "recipient".into(),
        recipient.clone().map(Value::String).unwrap_or(Value::Null),
    );
    payload.insert(
        "activeRecipient".into(),
        recipient.map(Value::String).unwrap_or(Value::Null),
    );
    payload.insert("sentCount".into(), json!(sent_count));
    payload.insert("failedCount".into(), json!(failed_count));
    payload.insert("currentIndex".into(), json!(current_index));
    payload.insert("total".into(), json!(total));
    payload.insert("remaining".into(), json!(remaining));
    payload.insert("timestamp".into(), json!(now));

    send_telemetry(shared, Value::Object(payload));
}

fn details(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

// --- Profiles ------------------------------------------------------------------

/// The Chromium flags desktop runtime launched each send window with. Keeping
/// background windows unthrottled is what made multi-window sends stay fast.
const PROFILE_ARGS: &[&str] = &[
    // `--no-startup-window` keeps Chromium from opening its own startup tab
    // (the ungoogled first-run page, or a new-tab page) before we attach. The
    // only window is then the one we open ourselves for Gmail, so the operator
    // never sees a stray first-run tab. (Dropping it re-introduced a stray
    // `chrome://newtab/` tab in testing.)
    "--no-startup-window",
    "--new-window",
    "--incognito",
    "--no-first-run",
    "--no-default-browser-check",
    "--disable-background-timer-throttling",
    "--disable-backgrounding-occluded-windows",
    "--disable-renderer-backgrounding",
    // Windows' native occlusion detector can still deprioritize a covered
    // headed window; Gmail sessions must keep running while another app is up.
    "--disable-features=CalculateNativeWinOcclusion,IntensiveWakeUpThrottling,TimerThrottlingForHiddenFrames",
    "--disable-ipc-flooding-protection",
    "--process-per-site",
    "--disable-extensions",
    "--disable-component-update",
    "--disable-default-apps",
    "--disable-sync",
    "--disable-background-networking",
    "--disable-breakpad",
    "--disable-client-side-phishing-detection",
    "--disable-domain-reliability",
    "--no-pings",
    "--metrics-recording-only",
    "--mute-audio",
    "--password-store=basic",
    "--use-mock-keychain",
    // --- Memory savings ---
    // No GPU process: saves ~50-100 MB per instance. Gmail renders fine
    // with software rasterization.
    "--disable-gpu",
    // Cap renderer processes at 2. Gmail typically has 1-2 tabs;
    // the default ~30 is excessive and wastes ~100-200 MB.
    "--renderer-process-limit=2",
    // Cap V8 heap at 512 MB per renderer. Gmail's JS heap rarely
    // exceeds 300 MB but the default (2-4 GB) invites bloat when
    // multiple instances are open.
    "--js-flags=--max-old-space-size=512",
    // Don't autorun utility services (network, storage, etc.).
    "--no-service-autorun",
];

pub(crate) fn profile_args() -> Vec<String> {
    let mut args: Vec<String> = PROFILE_ARGS.iter().map(|arg| (*arg).to_string()).collect();
    if cfg!(target_os = "linux") {
        args.push("--disable-dev-shm-usage".into());
    }
    args
}

/// A read-only view of one account, so long jobs never hold the state lock.
struct AccountSnap {
    tearing_down: bool,
    ready: bool,
    pending: bool,
    email: Option<String>,
    driver: Arc<dyn BrowserDriver>,
    page: PageId,
}

fn account_snapshot(shared: &Shared, id: &str) -> Option<AccountSnap> {
    let state = shared.state.lock().ok()?;
    let account = state.accounts.get(id)?;
    Some(AccountSnap {
        tearing_down: account.tearing_down,
        ready: account.ready,
        pending: matches!(account.login, LoginState::Pending),
        email: account.email.clone(),
        driver: account.driver.clone(),
        page: account.page,
    })
}

/// `addProfile`: launch one headed Chromium with a throwaway profile and start
/// watching for a Gmail sign-in.
async fn create_profile(shared: &Shared, options: Value) -> Result<Value, String> {
    let wait_for_login = options
        .get("waitForLogin")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let expected_email = options
        .get("expectedEmail")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_lowercase();

    if shared
        .state
        .lock()
        .map(|state| state.reset_in_progress)
        .unwrap_or(false)
    {
        return Err("A browser reset is in progress.".to_string());
    }

    let executable = paths::chromium_executable_path()?;
    if !executable.exists() {
        return Err(format!(
            "The bundled Clearcote Chromium was not found at {}. Reinstall Novamail or restore the vendor/clearcote folder.",
            executable.display()
        ));
    }
    log::info!(
        "[browser] Launching the bundled Clearcote Chromium: {}",
        executable.display()
    );

    let profile_dir: PathBuf = std::env::temp_dir().join(format!(
        "novamail-session-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&profile_dir).map_err(|error| error.to_string())?;

    // Resolve the device persona: a complete new machine identity (OS, screen, cores, GPU, touch)
    // drawn from the bundled catalogue, with the network identity (timezone, locale, geolocation,
    // WebRTC IP) pinned to the proxy's exit region. Merge its Clearcote engine switches into the
    // launch flags and hand the driver the same seed so the human-motion persona matches the
    // fingerprint.
    let persona = crate::persona::resolve(&options).await;
    log::info!(
        "[browser] Persona {} ({})",
        persona.spec.summary(),
        persona.spec.device_id
    );
    let persona_json = persona.spec.to_json();
    let mut merged_args = profile_args();
    merged_args.extend(persona.args.clone());

    let mut config = DriverConfig::new(executable)
        .headless(false)
        .user_data_dir(profile_dir.clone())
        .humanize(Some(persona.spec.seed.clone()))
        .args(merged_args);
    if let Some((width, height)) = persona.window {
        config = config.window_size(width, height);
    }
    let driver: Arc<dyn BrowserDriver> = match ChromiumDriver::launch(config).await {
        Ok(driver) => Arc::new(driver),
        Err(error) => {
            let _ = std::fs::remove_dir_all(&profile_dir);
            return Err(error);
        }
    };
    let page = match driver.new_page(None).await {
        Ok(page) => page,
        Err(error) => {
            let _ = driver.shutdown().await;
            let _ = std::fs::remove_dir_all(&profile_dir);
            return Err(error);
        }
    };

    let id = uuid::Uuid::new_v4().to_string();
    {
        let mut state = shared.state.lock().map_err(|error| error.to_string())?;
        state.accounts.insert(
            id.clone(),
            Account {
                id: id.clone(),
                email: None,
                name: "Clearcote Instance".to_string(),
                ready: false,
                expected_email: expected_email.clone(),
                driver,
                page,
                tearing_down: false,
                login: LoginState::Pending,
            },
        );
    }

    send_telemetry(
        shared,
        json!({
            "status": "waiting-for-login",
            "accountId": id,
            "instanceId": id,
            "accountEmail": expected_email,
            "processId": Value::Null,
            "activeRecipient": Value::Null,
            "persona": persona_json.clone(),
            "logs": format!(
                "Waiting for Gmail login on browser instance {id} ({}).",
                persona.spec.summary()
            ),
        }),
    );

    let watcher_shared = shared.clone();
    let watcher_id = id.clone();
    tauri::async_runtime::spawn(async move {
        watch_browser_close(watcher_shared, watcher_id).await;
    });

    let login_shared = shared.clone();
    let login_id = id.clone();
    tauri::async_runtime::spawn(async move {
        detect_login(login_shared, login_id).await;
    });

    if !wait_for_login {
        return Ok(json!({
            "id": id,
            "email": Value::Null,
            "name": "Clearcote Instance",
            "ready": false,
            "pending": true,
            "status": "waiting-for-login",
            "processId": Value::Null,
            "persona": persona_json,
        }));
    }

    loop {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let settled = shared
            .state
            .lock()
            .ok()
            .and_then(|state| {
                state
                    .accounts
                    .get(&id)
                    .map(|account| (account.ready, matches!(account.login, LoginState::Pending)))
            })
            .map(|(ready, pending)| ready || !pending)
            .unwrap_or(true);
        if settled {
            break;
        }
    }

    let (email, ready, name) = {
        let state = shared.state.lock().map_err(|error| error.to_string())?;
        match state.accounts.get(&id) {
            Some(account) => (account.email.clone(), account.ready, account.name.clone()),
            None => (None, false, "Clearcote Instance".to_string()),
        }
    };
    if ready {
        Ok(json!({
            "id": id,
            "email": email,
            "name": name,
            "ready": true,
            "status": "ready",
            "processId": Value::Null,
            "persona": persona_json.clone(),
        }))
    } else {
        Ok(json!({
            "id": id,
            "email": Value::Null,
            "name": name,
            "ready": false,
            "pending": true,
            "status": "waiting-for-login",
            "processId": Value::Null,
            "persona": persona_json,
        }))
    }
}

/// Notice when the operator closes Chromium directly. In an app process that
/// browser owns the whole app session, so remove its dashboard entry and exit.
async fn watch_browser_close(shared: Shared, id: String) {
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let Some(snapshot) = account_snapshot(&shared, &id) else {
            return;
        };
        if snapshot.tearing_down {
            return;
        }
        if !snapshot.driver.is_browser_closed() {
            continue;
        }

        log::info!("[browser] Chromium for app instance {id} was closed by the operator.");
        let app = shared.app.lock().ok().and_then(|guard| guard.clone());
        let app_role = !crate::runtime::RuntimeInfo::from_env().is_launcher;
        close_browser(&shared, &id, "browser-closed").await;
        if app_role {
            crate::control::report_browser_closed().await;
            if let Some(app) = app {
                crate::runtime::shutdown_app(&app).await;
            }
        }
        return;
    }
}

/// Background Gmail sign-in detection: navigate to the inbox, wait for it to
/// render, then read the signed-in address from the page.
async fn detect_login(shared: Shared, id: String) {
    let Some(snapshot) = account_snapshot(&shared, &id) else {
        return;
    };
    let expected = shared
        .state
        .lock()
        .ok()
        .and_then(|state| {
            state
                .accounts
                .get(&id)
                .map(|account| account.expected_email.clone())
        })
        .unwrap_or_default();

    if let Err(error) = snapshot
        .driver
        .goto(snapshot.page, "https://mail.google.com/mail/u/0/#inbox")
        .await
    {
        finish_login(
            &shared,
            &id,
            LoginState::Failed(error.clone()),
            "waiting-for-login",
            "waiting-for-login",
            &error,
        );
        return;
    }

    // Only treat the page as "signed in" once we are actually on Gmail with the
    // inbox rendered. A fresh throwaway profile starts logged out, so `#inbox`
    // redirects to the Google sign-in page on `accounts.google.com` — whose card
    // is wrapped in a `[role="main"]` container. Matching that container would
    // make detection give up (and mark the account failed) before the user ever
    // gets a chance to sign in, so gate the selector on the Gmail host first and
    // keep waiting until the inbox really is there.
    let selector = "(function(){if(location.hostname.toLowerCase()!=='mail.google.com')return false;\
                     return !!document.querySelector('#inbox, [role=\"main\"], [aria-label=\"Primary\"]');})()";
    // Wait generously: a real sign-in can include 2FA and take a few minutes.
    let mut inbox_ready = false;
    for _ in 0..600 {
        match snapshot.driver.evaluate(snapshot.page, selector).await {
            Ok(value) if value.as_bool() == Some(true) => {
                inbox_ready = true;
                break;
            }
            _ => tokio::time::sleep(Duration::from_millis(1000)).await,
        }
    }

    if !inbox_ready {
        let message = "Gmail did not finish loading after sign-in.".to_string();
        finish_login(
            &shared,
            &id,
            LoginState::Failed(message.clone()),
            "waiting-for-login",
            "waiting-for-login",
            &message,
        );
        return;
    }

    let script = "(function(){\
                  var emailPattern=/[A-Z0-9._%+-]+@[A-Z0-9.-]+\\.[A-Z]{2,}/i;\
                  var accountNodes=Array.from(document.querySelectorAll('[aria-label]'));\
                  for(var i=0;i<accountNodes.length;i++){\
                    var label=accountNodes[i].getAttribute('aria-label')||'';\
                    if(!/google account/i.test(label))continue;\
                    var accountMatch=label.match(emailPattern);\
                    if(accountMatch)return accountMatch[0];\
                  }\
                  var titleMatch=(document.title||'').match(emailPattern);\
                  return titleMatch?titleMatch[0]:'';})()";
    // Gmail's inbox container appears before the account menu is always
    // hydrated. Poll the identity briefly so slower secondary app instances
    // do not get marked disconnected while Gmail finishes rendering.
    let mut email = String::new();
    for _ in 0..60 {
        if let Ok(Value::String(candidate)) = snapshot.driver.evaluate(snapshot.page, script).await
        {
            if candidate.contains('@') {
                email = candidate;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    if !email.contains('@') {
        let message =
            "Gmail loaded, but the signed-in account could not be identified.".to_string();
        finish_login(
            &shared,
            &id,
            LoginState::Failed(message.clone()),
            "waiting-for-login",
            "waiting-for-login",
            &message,
        );
        return;
    }
    if !expected.is_empty() && email.to_lowercase() != expected {
        let message = format!(
            "This replacement instance is signed in to {email}. Sign in to {expected} to resume the unsent emails."
        );
        finish_login(
            &shared,
            &id,
            LoginState::Failed(message.clone()),
            "error",
            "login-error",
            &message,
        );
        return;
    }

    {
        let Ok(mut state) = shared.state.lock() else {
            return;
        };
        if let Some(account) = state.accounts.get_mut(&id) {
            account.email = Some(email.clone());
            account.name = email.clone();
            account.ready = true;
            account.login = LoginState::Ready;
        }
    }
    send_telemetry(
        &shared,
        json!({
            "status": "ready",
            "accountId": id,
            "instanceId": id,
            "accountEmail": email,
            "processId": Value::Null,
            "activeRecipient": Value::Null,
            "logs": [format!("{email} is ready")],
        }),
    );
    crate::control::report_gmail_account(&email);
}

fn finish_login(
    shared: &Shared,
    id: &str,
    login: LoginState,
    status: &str,
    event: &str,
    message: &str,
) {
    let email = {
        let Ok(mut state) = shared.state.lock() else {
            return;
        };
        let Some(account) = state.accounts.get_mut(id) else {
            return;
        };
        account.login = login;
        account
            .email
            .clone()
            .unwrap_or_else(|| account.expected_email.clone())
    };
    send_telemetry(
        shared,
        json!({
            "status": status,
            "event": event,
            "accountId": id,
            "instanceId": id,
            "accountEmail": email,
            "processId": Value::Null,
            "activeRecipient": Value::Null,
            "logs": message,
        }),
    );
}

/// Tear one instance down: stop its worker, close the browser (which removes the
/// throwaway profile directory) and forget the account.
async fn close_browser(shared: &Shared, id: &str, status: &str) {
    let (driver, worker) = {
        let Ok(mut state) = shared.state.lock() else {
            return;
        };
        let Some(account) = state.accounts.get_mut(id) else {
            return;
        };
        if account.tearing_down {
            return;
        }
        account.tearing_down = true;
        account.ready = false;
        (
            account.driver.clone(),
            state.active_workers.get(id).cloned(),
        )
    };

    if let Some(worker) = &worker {
        if let Ok(mut guard) = worker.lock() {
            guard.aborted = true;
            guard.paused = false;
        }
    }

    let event = worker.as_ref().and_then(|worker| {
        worker.lock().ok().map(|guard| {
            json!({
                "status": status,
                "event": status,
                "instanceId": guard.instance_id,
                "accountId": guard.instance_id,
                "accountEmail": guard.account_email,
                "processId": Value::Null,
                "activeRecipient": guard.current_recipient,
                "recipientId": guard.queue.get(guard.current_index).and_then(|item| item.get("id")),
                "currentIndex": guard.current_index,
                "total": guard.total,
                "sentCount": guard.sent_count,
                "failedCount": guard.failed_count,
                "logs": format!("browser instance {} closed.", guard.instance_id),
            })
        })
    });
    if let Some(event) = event {
        send_telemetry(shared, event);
    }

    let _ = driver.shutdown().await;

    if let Ok(mut state) = shared.state.lock() {
        state.accounts.remove(id);
        state.active_workers.remove(id);

        let mut empty_runs = Vec::new();
        for (session_id, run) in &state.runs {
            let Ok(mut workers) = run.workers.lock() else {
                continue;
            };
            let mut closed_recipient_ids = HashSet::new();
            let mut closed_recipient_emails = HashSet::new();
            workers.retain(|worker| {
                let Ok(mut guard) = worker.lock() else {
                    return true;
                };
                if guard.instance_id != id {
                    return true;
                }

                // A task already in flight may still hold this worker until its
                // browser operation unwinds. Drop all browser-owned queue data
                // now so that Arc does not retain recipient details meanwhile.
                for recipient in guard.queue.iter().chain(guard.retry_queue.iter()) {
                    if let Some(recipient_id) = recipient.get("id").and_then(Value::as_str) {
                        closed_recipient_ids.insert(recipient_id.to_string());
                    }
                    if let Some(email) = recipient.get("email").and_then(Value::as_str) {
                        closed_recipient_emails.insert(email.trim().to_ascii_lowercase());
                    }
                }
                guard.queue = Vec::new();
                guard.retry_queue = Vec::new();
                guard.instance_id = String::new();
                guard.worker_id = String::new();
                guard.slot_id = String::new();
                guard.account_email = String::new();
                guard.process_id = None;
                guard.current_recipient = None;
                guard.error_message = None;
                false
            });
            workers.shrink_to_fit();
            if let Ok(mut payload) = run.payload.lock() {
                if let Some(recipients) =
                    payload.get_mut("recipients").and_then(Value::as_array_mut)
                {
                    recipients.retain(|recipient| {
                        let id_matches = recipient
                            .get("id")
                            .and_then(Value::as_str)
                            .map(|recipient_id| closed_recipient_ids.contains(recipient_id))
                            .unwrap_or(false);
                        let email_matches = recipient
                            .get("email")
                            .and_then(Value::as_str)
                            .map(|email| {
                                closed_recipient_emails.contains(&email.trim().to_ascii_lowercase())
                            })
                            .unwrap_or(false);
                        !id_matches && !email_matches
                    });
                    recipients.shrink_to_fit();
                }
                if let Some(assignments) =
                    payload.get_mut("assignments").and_then(Value::as_array_mut)
                {
                    assignments.retain(|assignment| {
                        assignment
                            .get("instanceId")
                            .or_else(|| assignment.get("accountId"))
                            .and_then(Value::as_str)
                            != Some(id)
                    });
                    assignments.shrink_to_fit();
                }
            }
            if workers.is_empty() {
                empty_runs.push(session_id.clone());
            }
        }

        for session_id in empty_runs {
            state.runs.remove(&session_id);
            state.active_sessions.remove(&session_id);
            state.run_order.retain(|candidate| candidate != &session_id);
        }
        let retry_key_suffix = format!(":{id}");
        state
            .retry_locks
            .retain(|key| !key.ends_with(&retry_key_suffix));
        state.accounts.shrink_to_fit();
        state.active_workers.shrink_to_fit();
        state.runs.shrink_to_fit();
        state.active_sessions.shrink_to_fit();
        state.run_order.shrink_to_fit();
        state.retry_locks.shrink_to_fit();
    }

    forget_persisted_browser_data(shared, id);
}

/// Remove references to a closed instance from saved campaign/account data.
/// User-authored campaign content and recipient lists remain untouched.
fn forget_persisted_browser_data(shared: &Shared, id: &str) {
    let app = shared.app.lock().ok().and_then(|guard| guard.clone());
    let Some(app) = app else { return };
    let Some(store) = app.try_state::<Mutex<Store>>() else {
        return;
    };
    let Ok(mut store) = store.lock() else { return };

    if let Err(error) = store.transact(|data| {
        data.remove(&format!("accounts:{id}"));
        if let Some(accounts) = data.get_mut("accounts") {
            match accounts {
                Value::Array(accounts) => {
                    accounts.retain(|account| account.get("id").and_then(Value::as_str) != Some(id))
                }
                Value::Object(accounts) => {
                    accounts.remove(id);
                }
                _ => {}
            }
        }

        if let Some(campaigns) = data.get_mut("campaigns").and_then(Value::as_array_mut) {
            for campaign in campaigns {
                if let Some(object) = campaign.as_object_mut() {
                    let belongs_to_closed_instance =
                        object.get("accountId").and_then(Value::as_str) == Some(id);
                    if belongs_to_closed_instance {
                        object.insert("accountId".to_string(), Value::Null);
                        if object.contains_key("accountEmail") {
                            object.insert("accountEmail".to_string(), Value::Null);
                        }
                    }
                }
                if let Some(settings) = campaign
                    .get_mut("instanceSettings")
                    .and_then(Value::as_array_mut)
                {
                    for slot in settings {
                        if let Some(object) = slot.as_object_mut() {
                            if object.get("accountId").and_then(Value::as_str) == Some(id) {
                                object.insert("accountId".to_string(), Value::Null);
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }) {
        log::warn!("Could not clear stored references to closed browser {id}: {error}");
    }
}

// --- Worker loop ---------------------------------------------------------------

/// `resolveWorkerAccount`: wait for (or reject) this instance's Gmail login and
/// hand back the browser + tab to send from.
async fn resolve_worker_account(
    shared: &Shared,
    session_id: &str,
    campaign_id: &str,
    worker: &Arc<Mutex<Worker>>,
) -> Result<(Arc<dyn BrowserDriver>, PageId, String), String> {
    let instance_id = worker
        .lock()
        .map_err(|error| error.to_string())?
        .instance_id
        .clone();
    let mut announced = false;
    loop {
        let snapshot = account_snapshot(shared, &instance_id)
            .ok_or_else(|| format!("browser instance {instance_id} is no longer available."))?;
        if snapshot.tearing_down {
            return Err(format!("browser instance {instance_id} is disconnected."));
        }
        if snapshot.ready {
            return Ok((
                snapshot.driver,
                snapshot.page,
                snapshot.email.unwrap_or_default(),
            ));
        }
        if snapshot.pending {
            if !announced {
                announced = true;
                let label = snapshot
                    .email
                    .clone()
                    .unwrap_or_else(|| instance_id.clone());
                emit_worker(
                    shared,
                    session_id,
                    campaign_id,
                    worker,
                    "waiting-for-login",
                    details(json!({
                        "recipient": Value::Null,
                        "logs": format!("Waiting for Gmail login on {label}."),
                    })),
                );
            }
            if worker.lock().map(|guard| guard.aborted).unwrap_or(true) {
                return Err("Automation stopped while waiting for Gmail login.".to_string());
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
            continue;
        }
        // The sign-in watch settled without ever reporting an account; surface
        // why, so the worker's final `error` telemetry explains itself.
        let reason = shared
            .state
            .lock()
            .ok()
            .and_then(|state| {
                state
                    .accounts
                    .get(&instance_id)
                    .map(|account| match &account.login {
                        LoginState::Failed(message) => message.clone(),
                        _ => String::new(),
                    })
            })
            .unwrap_or_default();
        if reason.is_empty() {
            return Err(format!(
                "Gmail login is required for browser instance {instance_id}."
            ));
        }
        return Err(format!(
            "Gmail login is required for browser instance {instance_id}: {reason}"
        ));
    }
}

/// `waitUntilResumed`: block while paused, then announce the resume.
async fn wait_until_resumed(
    shared: &Shared,
    session_id: &str,
    campaign_id: &str,
    worker: &Arc<Mutex<Worker>>,
) {
    loop {
        let paused = worker.lock().map(|guard| guard.paused).unwrap_or(false);
        let aborted = worker.lock().map(|guard| guard.aborted).unwrap_or(true);
        if !paused || aborted {
            break;
        }
        if let Ok(mut guard) = worker.lock() {
            guard.status = "paused".to_string();
        }
        emit_worker(
            shared,
            session_id,
            campaign_id,
            worker,
            "paused",
            details(
                json!({ "recipient": Value::Null, "logs": "Worker paused; its queue is preserved." }),
            ),
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    if !worker.lock().map(|guard| guard.aborted).unwrap_or(true) {
        if let Ok(mut guard) = worker.lock() {
            guard.status = "running".to_string();
        }
        emit_worker(
            shared,
            session_id,
            campaign_id,
            worker,
            "running",
            details(json!({ "recipient": Value::Null, "logs": "Worker resumed." })),
        );
    }
}

/// `compactRetry` + the payload fallbacks desktop runtime recorded for a failed send.
fn build_retry_entry(recipient: &Value, payload: &Value) -> Value {
    let mut object = recipient.as_object().cloned().unwrap_or_default();
    object.insert(
        "subject".into(),
        payload
            .get("subject")
            .or_else(|| payload.get("subjectTemplate"))
            .cloned()
            .unwrap_or(Value::Null),
    );
    object.insert(
        "html".into(),
        payload.get("html").cloned().unwrap_or(Value::Null),
    );
    object.insert(
        "text".into(),
        payload.get("text").cloned().unwrap_or(Value::Null),
    );
    object.insert(
        "attachments".into(),
        payload
            .get("attachments")
            .cloned()
            .unwrap_or_else(|| json!([])),
    );
    Value::Object(object)
}

/// Return only the shared message fields a sender or retry needs. Recipient
/// queues and browser assignments stay in the run record so close can erase
/// the portion owned by one instance without copying unrelated recipients into
/// an in-flight send request.
///
/// `outputFormat`, `fileNameTemplate` and `autoGenerateFile` travel too: the
/// sender needs them to decide the body shape (HTML inline for the inline
/// formats) and which per-recipient document to attach. Without them the send
/// request saw a blank format, fell back to `format = "inline"`, and produced
/// exactly the bug a plain text body with a stray generated `.html` file
/// attached describes.
fn run_content_payload(run: &RunRecord) -> Value {
    let mut content = Map::new();
    let Ok(payload) = run.payload.lock() else {
        return Value::Object(content);
    };
    for key in [
        "subject",
        "subjectTemplate",
        "html",
        "inlineHtml",
        "text",
        "attachments",
        "variables",
        "outputFormat",
        "fileNameTemplate",
        "autoGenerateFile",
    ] {
        if let Some(value) = payload.get(key) {
            content.insert(key.to_string(), value.clone());
        }
    }
    Value::Object(content)
}

/// `runWorker`: send this instance's queue, one recipient at a time.
async fn run_worker(shared: Shared, run: Arc<RunRecord>, worker: Arc<Mutex<Worker>>) {
    let session_id = run.session_id.clone();
    let campaign_id = run.campaign_id.clone();

    let (driver, page, account_email) =
        match resolve_worker_account(&shared, &session_id, &campaign_id, &worker).await {
            Ok(resolved) => resolved,
            Err(error) => {
                {
                    let Ok(mut guard) = worker.lock() else { return };
                    guard.status = "error".to_string();
                    guard.error_message = Some(error.clone());
                }
                emit_worker(
                    &shared,
                    &session_id,
                    &campaign_id,
                    &worker,
                    "error",
                    details(json!({
                        "recipient": Value::Null,
                        "error": error,
                        "logs": format!("Instance worker error: {error}"),
                    })),
                );
                finalize_worker(&shared, &worker).await;
                return;
            }
        };

    {
        let Ok(mut guard) = worker.lock() else { return };
        guard.account_email = account_email.clone();
        guard.status = "running".to_string();
    }
    emit_worker(
        &shared,
        &session_id,
        &campaign_id,
        &worker,
        "running",
        details(json!({ "recipient": Value::Null, "logs": "Automation started." })),
    );

    let queue_len = worker.lock().map(|guard| guard.queue.len()).unwrap_or(0);
    for index in 0..queue_len {
        if worker.lock().map(|guard| guard.aborted).unwrap_or(true) {
            break;
        }
        wait_until_resumed(&shared, &session_id, &campaign_id, &worker).await;
        if worker.lock().map(|guard| guard.aborted).unwrap_or(true) {
            break;
        }
        // A pause that landed mid-send may have abandoned the message while
        // Gmail's compose still held its half-built draft. Close any leftover
        // compose once on resume so the next message starts from a clean
        // window (this runs after resume, never while paused).
        if worker.lock().map(|guard| guard.compose_dirty).unwrap_or(false) {
            let recovered = mail_sender::recover_after_uncertain_send(&driver, page)
                .await
                .is_ok();
            if let Ok(mut guard) = worker.lock() {
                guard.compose_dirty = !recovered;
            }
        }

        let recipient = match worker.lock() {
            Ok(guard) => guard.queue.get(index).cloned().unwrap_or(Value::Null),
            Err(_) => break,
        };
        let recipient_id = recipient
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let email = recipient
            .get("email")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        {
            let Ok(mut guard) = worker.lock() else { break };
            guard.current_recipient = Some(email.clone());
            guard.current_index = index;
        }

        emit_worker(
            &shared,
            &session_id,
            &campaign_id,
            &worker,
            "waiting",
            details(json!({
                "recipientId": recipient_id,
                "currentIndex": index,
                "logs": format!("Waiting to process {email}."),
            })),
        );
        emit_worker(
            &shared,
            &session_id,
            &campaign_id,
            &worker,
            "sending",
            details(json!({
                "recipientId": recipient_id,
                "currentIndex": index,
                "logs": format!("Opening the compose window for {email}."),
            })),
        );

        let request = SendRequest {
            session_id: session_id.clone(),
            campaign_id: campaign_id.clone(),
            instance_id: worker
                .lock()
                .map(|guard| guard.instance_id.clone())
                .unwrap_or_default(),
            worker_id: worker
                .lock()
                .map(|guard| guard.worker_id.clone())
                .unwrap_or_default(),
            slot_id: worker
                .lock()
                .map(|guard| guard.slot_id.clone())
                .unwrap_or_default(),
            account_email: account_email.clone(),
            recipient: recipient.clone(),
            recipient_id: recipient
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            index,
            total: queue_len,
            payload: Arc::new(run_content_payload(&run)),
            driver: driver.clone(),
            page,
            app: shared.app.lock().ok().and_then(|guard| guard.clone()),
        };

        // Strong pause: a pause request must not wait for the in-flight send.
        // While the message is going out, the flag is polled every 150 ms; the
        // moment pause lands the send future is dropped — no more browser
        // steps, no waiting — and the recipient is recorded as failed, then
        // telemetry settles immediately.
        let pause_watch = {
            let worker = Arc::clone(&worker);
            async move {
                loop {
                    if worker.lock().map(|guard| guard.paused).unwrap_or(false) {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(150)).await;
                }
            }
        };
        let send_result = tokio::select! {
            result = tokio::time::timeout(
                SEND_OPERATION_TIMEOUT,
                shared.sender.send(request),
            ) => match result {
                Ok(result) => result,
                Err(_) => {
                    let reason = "Gmail did not confirm whether this message was accepted before the 3-minute send timeout. Check Gmail Sent before retrying.";
                    let reason = match mail_sender::recover_after_uncertain_send(&driver, page).await {
                        Ok(()) => reason.to_string(),
                        Err(recovery) => {
                            format!("{reason} The old compose could not be closed: {recovery}")
                        }
                    };
                    Err(SendFailure::Uncertain(reason))
                }
            },
            () = pause_watch => {
                if let Ok(mut guard) = worker.lock() {
                    guard.compose_dirty = true;
                }
                Err(SendFailure::Uncertain(
                    "Sending was interrupted: pause was requested while this message was still going out, so it is recorded as failed.".to_string(),
                ))
            }
        };

        match send_result {
            Ok(report) => {
                if let Ok(mut guard) = worker.lock() {
                    guard.sent_count += 1;
                    guard.current_index = index + 1;
                    // The send step may have resolved a real subject (e.g. a
                    // rendered template); record it against the recipient the
                    // way desktop runtime's `finalizeRecipient` did, so retries and the
                    // run summary report what was actually sent.
                    if let Some(subject) = report.subject {
                        if let Some(Value::Object(object)) = guard.queue.get_mut(index) {
                            object.insert("subject".into(), json!(subject));
                        }
                    }
                }
                emit_worker(
                    &shared,
                    &session_id,
                    &campaign_id,
                    &worker,
                    "sent",
                    details(json!({
                        "recipientId": recipient_id,
                        "currentIndex": index + 1,
                        "logs": format!("Sent to {email}."),
                    })),
                );
            }
            Err(SendFailure::Recipient(reason)) => {
                if let Ok(mut guard) = worker.lock() {
                    if !guard.instance_id.is_empty() {
                        guard.failed_count += 1;
                        guard.current_index = index + 1;
                        let retry = build_retry_entry(&recipient, &run_content_payload(&run));
                        guard.retry_queue.push(retry);
                    }
                }
                emit_worker(
                    &shared,
                    &session_id,
                    &campaign_id,
                    &worker,
                    "failed",
                    details(json!({
                        "recipientId": recipient_id,
                        "currentIndex": index + 1,
                        "error": reason,
                        "logs": format!("Failed for {email}: {reason}"),
                    })),
                );
            }
            Err(SendFailure::Uncertain(reason)) => {
                if let Ok(mut guard) = worker.lock() {
                    if !guard.instance_id.is_empty() {
                        guard.failed_count += 1;
                        guard.current_index = index + 1;
                        let retry = build_retry_entry(&recipient, &run_content_payload(&run));
                        guard.retry_queue.push(retry);
                    }
                }
                emit_worker(
                    &shared,
                    &session_id,
                    &campaign_id,
                    &worker,
                    "failed",
                    details(json!({
                        "recipientId": recipient_id,
                        "currentIndex": index + 1,
                        "error": reason,
                        "logs": format!("Gmail did not confirm delivery to {email}; marked failed and continuing with the next recipient."),
                    })),
                );
            }
            Err(SendFailure::Stop(reason)) => {
                if let Ok(mut guard) = worker.lock() {
                    if !guard.instance_id.is_empty() {
                        guard.failed_count += 1;
                        guard.current_index = index + 1;
                        guard.error_message = Some(if reason.contains("may have been accepted") {
                            format!(
                                "Gmail could not confirm delivery to {email}. Check Gmail Sent before retrying."
                            )
                        } else {
                            format!(
                                "Gmail compose was kept open after a failure for {email}. Review or send that draft, then retry the remaining recipients."
                            )
                        });
                        let retry = build_retry_entry(&recipient, &run_content_payload(&run));
                        guard.retry_queue.push(retry);
                    }
                }
                emit_worker(
                    &shared,
                    &session_id,
                    &campaign_id,
                    &worker,
                    "failed",
                    details(json!({
                        "recipientId": recipient_id,
                        "currentIndex": index + 1,
                        "error": reason,
                        "logs": format!("Failed for {email}: {reason}"),
                    })),
                );
                break;
            }
        }

        if let Ok(mut guard) = worker.lock() {
            guard.current_recipient = None;
        }
    }

    finalize_worker(&shared, &worker).await;
}

/// The `finally` block of `runWorker`: fold the unsent tail into the retry
/// queue, record the final status, deregister the worker and emit the summary.
async fn finalize_worker(shared: &Shared, worker: &Arc<Mutex<Worker>>) {
    let (status, remaining, error_message, current_index, instance_id, session_id, campaign_id) = {
        let Ok(mut guard) = worker.lock() else { return };
        let queued: HashSet<String> = guard
            .retry_queue
            .iter()
            .filter_map(|recipient| {
                recipient
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect();
        let tail: Vec<Value> = guard
            .queue
            .iter()
            .skip(guard.current_index)
            .filter(|recipient| {
                let id = recipient.get("id").and_then(Value::as_str).unwrap_or("");
                !queued.contains(id)
            })
            .cloned()
            .collect();
        guard.retry_queue.extend(tail);

        // A terminal worker event must not keep displaying its last recipient
        // as though that address were still being processed.
        guard.current_recipient = None;
        let aborted = guard.aborted;
        let error = guard.error_message.clone();
        guard.status = if aborted {
            "aborted".to_string()
        } else if error.is_some() {
            "error".to_string()
        } else {
            "completed".to_string()
        };
        guard.paused = false;

        let status = guard.status.clone();
        let remaining = guard.retry_queue.len();
        let error_message = if status == "error" {
            error.unwrap_or_default()
        } else {
            String::new()
        };
        (
            status,
            remaining,
            error_message,
            guard.current_index,
            guard.instance_id.clone(),
            guard.session_id.clone(),
            guard.campaign_id.clone(),
        )
    };

    if let Ok(mut state) = shared.state.lock() {
        let remove = state
            .active_workers
            .get(&instance_id)
            .map(|active| Arc::ptr_eq(active, worker))
            .unwrap_or(false);
        if remove {
            state.active_workers.remove(&instance_id);
        }
    }

    let logs = match status.as_str() {
        "completed" => "Instance queue completed.".to_string(),
        "aborted" => {
            "Instance queue stopped; unsent recipients are available to retry.".to_string()
        }
        _ => error_message.clone(),
    };
    emit_worker(
        shared,
        &session_id,
        &campaign_id,
        worker,
        &status,
        details(json!({
            "recipient": Value::Null,
            "currentIndex": current_index,
            "remaining": remaining,
            "error": error_message,
            "logs": logs,
        })),
    );
}

// --- Normalization -------------------------------------------------------------

fn normalize_recipient(recipient: &Value) -> Value {
    match recipient {
        Value::String(email) => json!({ "email": email }),
        Value::Object(map) if map.get("email").and_then(Value::as_str).is_some() => {
            recipient.clone()
        }
        Value::Object(map) => Value::Object(map.clone()),
        _ => json!({}),
    }
}

// --- start ---------------------------------------------------------------------

async fn start_run(shared: Shared, payload: Value) -> Result<Value, String> {
    if shared
        .state
        .lock()
        .map(|state| state.reset_in_progress)
        .unwrap_or(false)
    {
        return Err("A browser reset is in progress.".to_string());
    }

    let recipients_value = payload.get("recipients").and_then(Value::as_array);
    let Some(recipients_raw) = recipients_value else {
        return Err("At least one recipient is required.".to_string());
    };
    if recipients_raw.is_empty() {
        return Err("At least one recipient is required.".to_string());
    }
    let recipients: Vec<Value> = recipients_raw.iter().map(normalize_recipient).collect();

    let assignments = payload
        .get("assignments")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if assignments.is_empty() {
        return Err(
            "Assign recipients to at least one browser instance before starting.".to_string(),
        );
    }

    let campaign_id = payload
        .get("campaignId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let session_id = uuid::Uuid::new_v4().to_string();

    let mut offset = 0usize;
    let mut assigned = HashSet::new();
    let mut workers: Vec<Arc<Mutex<Worker>>> = Vec::new();

    for assignment in &assignments {
        let instance_id = assignment
            .get("instanceId")
            .and_then(Value::as_str)
            .or_else(|| assignment.get("accountId").and_then(Value::as_str))
            .unwrap_or("")
            .to_string();
        if instance_id.is_empty() {
            return Err("Every automation assignment needs a browser instance ID.".to_string());
        }
        let limit = match assignment.get("limit") {
            Some(Value::Number(number)) => number.as_u64(),
            Some(Value::String(text)) => text.trim().parse::<u64>().ok(),
            _ => None,
        };
        let Some(limit) = limit else {
            return Err(format!(
                "Recipient limit for browser instance {instance_id} must be a non-negative integer."
            ));
        };
        if limit > MAX_RECIPIENTS_PER_INSTANCE {
            return Err("An instance cannot handle more than 500 emails.".to_string());
        }
        if !assigned.insert(instance_id.clone()) {
            return Err(format!(
                "browser instance {instance_id} is assigned more than once in this run."
            ));
        }

        {
            let state = shared.state.lock().map_err(|error| error.to_string())?;
            if !state.accounts.contains_key(&instance_id) {
                return Err(format!(
                    "browser instance {instance_id} is not available. Launch it manually before starting processing."
                ));
            }
            if state.active_workers.contains_key(&instance_id) {
                return Err(format!(
                    "browser instance {instance_id} already has an active worker."
                ));
            }
        }

        let limit = limit as usize;
        if offset + limit > recipients.len() {
            return Err(format!(
                "The allocation for browser instance {instance_id} exceeds the {} available recipients.",
                recipients.len()
            ));
        }

        let queue: Vec<Value> = recipients[offset..offset + limit]
            .iter()
            .map(|recipient| {
                let mut object = recipient.as_object().cloned().unwrap_or_default();
                let has_id = object
                    .get("id")
                    .and_then(Value::as_str)
                    .map(|id| !id.trim().is_empty())
                    .unwrap_or(false);
                if !has_id {
                    object.insert("id".into(), json!(uuid::Uuid::new_v4().to_string()));
                }
                object.insert("accountId".into(), json!(instance_id));
                object.insert("instanceId".into(), json!(instance_id));
                Value::Object(object)
            })
            .collect();
        offset += limit;

        let slot_id = assignment
            .get("slotId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        workers.push(Arc::new(Mutex::new(Worker {
            session_id: session_id.clone(),
            campaign_id: campaign_id.clone(),
            instance_id: instance_id.clone(),
            worker_id: uuid::Uuid::new_v4().to_string(),
            slot_id,
            account_email: String::new(),
            process_id: None,
            total: queue.len() as i64,
            queue,
            retry_queue: Vec::new(),
            current_index: 0,
            current_recipient: None,
            sent_count: 0,
            failed_count: 0,
            status: "queued".to_string(),
            paused: false,
            aborted: false,
            compose_dirty: false,
            error_message: None,
            updated_at: now_ms(),
        })));
    }

    if offset < recipients.len() {
        return Err(format!(
            "{} emails remain unassigned. Add another instance or adjust the allocation.",
            recipients.len() - offset
        ));
    }

    let run = Arc::new(RunRecord {
        session_id: session_id.clone(),
        campaign_id,
        payload: Mutex::new(payload),
        workers: Mutex::new(workers.clone()),
    });

    {
        let mut state = shared.state.lock().map_err(|error| error.to_string())?;
        for worker in &workers {
            if let Ok(guard) = worker.lock() {
                state
                    .active_workers
                    .insert(guard.instance_id.clone(), worker.clone());
            }
        }
        state.runs.insert(session_id.clone(), run.clone());
        state.active_sessions.insert(session_id.clone());
        state.run_order.push_back(session_id.clone());
        while state.run_order.len() > COMPLETED_RUN_HISTORY {
            if let Some(oldest) = state.run_order.pop_front() {
                state.runs.remove(&oldest);
                state.active_sessions.remove(&oldest);
            }
        }
    }

    let mut handles = Vec::new();
    for worker in &workers {
        handles.push(tauri::async_runtime::spawn(run_worker(
            shared.clone(),
            run.clone(),
            worker.clone(),
        )));
    }
    for handle in handles {
        let _ = handle.await;
    }

    {
        let mut state = shared.state.lock().map_err(|error| error.to_string())?;
        state.active_sessions.remove(&session_id);
    }

    let summary = build_run_summary(&run);
    Ok(summary)
}

fn build_run_summary(run: &RunRecord) -> Value {
    let mut sent_count = 0i64;
    let mut failed_count = 0i64;
    let mut total = 0i64;
    let mut aborted = false;
    let mut failed_emails: Vec<Value> = Vec::new();
    let content_payload = run_content_payload(run);

    if let Ok(workers) = run.workers.lock() {
        for worker in workers.iter() {
            let Ok(guard) = worker.lock() else { continue };
            sent_count += guard.sent_count;
            failed_count += guard.failed_count;
            total += guard.total;
            if guard.status == "aborted" {
                aborted = true;
            }
            for recipient in &guard.retry_queue {
                let pick = |key: &str| -> Value {
                    recipient
                        .get(key)
                        .or_else(|| content_payload.get(key))
                        .cloned()
                        .unwrap_or(Value::Null)
                };
                failed_emails.push(json!({
                    "email": recipient.get("email"),
                    "subject": pick("subject"),
                    "html": pick("html"),
                    "text": pick("text"),
                    "attachments": recipient
                        .get("attachments")
                        .or_else(|| content_payload.get("attachments"))
                        .cloned()
                        .unwrap_or_else(|| json!([])),
                    "instanceId": guard.instance_id,
                    "recipientId": recipient.get("id"),
                }));
            }
        }
    }

    json!({
        "sessionId": run.session_id,
        "sentCount": sent_count,
        "failedCount": failed_count,
        "total": total,
        "aborted": aborted,
        "failedEmails": failed_emails,
    })
}

// --- control -------------------------------------------------------------------

fn control_workers(shared: &Shared, target_value: &Value, action: &str) -> Result<Value, String> {
    let target = match target_value {
        Value::String(text) => text.clone(),
        Value::Object(map) => map
            .get("instanceId")
            .and_then(Value::as_str)
            .or_else(|| map.get("sessionId").and_then(Value::as_str))
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    };
    if target.is_empty() {
        return Err(format!(
            "An instance or automation session ID is required to {action}."
        ));
    }

    let workers: Vec<Arc<Mutex<Worker>>> = {
        let Ok(state) = shared.state.lock() else {
            return Err("Automation state is unavailable.".to_string());
        };
        if let Some(worker) = state.active_workers.get(&target) {
            vec![worker.clone()]
        } else if let Some(run) = state.runs.get(&target) {
            run.workers
                .lock()
                .map(|run_workers| {
                    run_workers
                        .iter()
                        .filter(|worker| {
                            let instance_id = worker
                                .lock()
                                .map(|guard| guard.instance_id.clone())
                                .unwrap_or_default();
                            state
                                .active_workers
                                .get(&instance_id)
                                .map(|active| Arc::ptr_eq(active, worker))
                                .unwrap_or(false)
                        })
                        .cloned()
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        }
    };

    let count = workers.len();
    for worker in &workers {
        match action {
            "stop" => {
                if let Ok(mut guard) = worker.lock() {
                    guard.aborted = true;
                    guard.paused = false;
                }
                let (session_id, campaign_id) = worker
                    .lock()
                    .map(|guard| (guard.session_id.clone(), guard.campaign_id.clone()))
                    .unwrap_or_default();
                if session_id.is_empty() {
                    continue;
                }
                emit_worker(
                    shared,
                    &session_id,
                    &campaign_id,
                    worker,
                    "stopping",
                    details(json!({
                        "recipient": Value::Null,
                        "logs": "Stop requested; the current message will finish before the queue pauses.",
                    })),
                );
            }
            "pause" => {
                if let Ok(mut guard) = worker.lock() {
                    guard.paused = true;
                }
                let (session_id, campaign_id) = worker
                    .lock()
                    .map(|guard| (guard.session_id.clone(), guard.campaign_id.clone()))
                    .unwrap_or_default();
                emit_worker(
                    shared,
                    &session_id,
                    &campaign_id,
                    worker,
                    "pausing",
                    details(json!({
                        "recipient": Value::Null,
                        "logs": "Pause requested; the message being sent is abandoned and counted as failed.",
                    })),
                );
            }
            _ => {
                if let Ok(mut guard) = worker.lock() {
                    guard.paused = false;
                }
            }
        }
    }

    Ok(json!({ "updated": count > 0, "count": count, "target": target }))
}

// --- retry ---------------------------------------------------------------------

struct RetryLock {
    shared: Shared,
    keys: Vec<String>,
}

impl Drop for RetryLock {
    fn drop(&mut self) {
        if let Ok(mut state) = self.shared.state.lock() {
            for key in &self.keys {
                state.retry_locks.remove(key);
            }
        }
    }
}

async fn retry(
    shared: Shared,
    session_id: String,
    instance_id: Option<String>,
    mode: String,
) -> Result<Value, String> {
    let instance_filter = instance_id.filter(|value| !value.is_empty());
    let (previous, workers) = {
        let Ok(state) = shared.state.lock() else {
            return Err("Automation state is unavailable.".to_string());
        };
        let Some(previous) = state.runs.get(&session_id).cloned() else {
            return Err("This automation session is no longer available for retry.".to_string());
        };
        let retryable = ["completed", "aborted", "error"];
        let workers: Vec<Arc<Mutex<Worker>>> = previous
            .workers
            .lock()
            .map(|previous_workers| {
                previous_workers
                    .iter()
                    .filter(|worker| {
                        worker
                            .lock()
                            .map(|guard| {
                                instance_filter
                                    .as_ref()
                                    .map(|filter| &guard.instance_id == filter)
                                    .unwrap_or(true)
                                    && retryable.contains(&guard.status.as_str())
                                    && !guard.retry_queue.is_empty()
                            })
                            .unwrap_or(false)
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        (previous, workers)
    };

    if workers.is_empty() {
        return Ok(json!({
            "skipped": true,
            "sessionId": session_id,
            "instanceId": instance_filter.clone().map(Value::String).unwrap_or(Value::Null),
        }));
    }

    let keys: Vec<String> = workers
        .iter()
        .map(|worker| {
            let id = worker
                .lock()
                .map(|guard| guard.instance_id.clone())
                .unwrap_or_default();
            format!("{session_id}:{id}")
        })
        .collect();
    {
        let mut state = shared.state.lock().map_err(|error| error.to_string())?;
        if keys.iter().any(|key| state.retry_locks.contains(key)) {
            return Ok(json!({
                "skipped": true,
                "sessionId": session_id,
                "instanceId": instance_filter.clone().map(Value::String).unwrap_or(Value::Null),
                "reason": "retry-in-progress",
            }));
        }
        for key in &keys {
            state.retry_locks.insert(key.clone());
        }
    }
    let _guard = RetryLock {
        shared: shared.clone(),
        keys,
    };

    let mut retry_recipients: Vec<Value> = Vec::new();
    let mut retry_assignments: Vec<Value> = Vec::new();
    let mut created_instance_ids: Vec<String> = Vec::new();

    for worker in &workers {
        let (worker_instance, worker_email, slot_id, retry_count, session_key, campaign_key) = {
            let Ok(guard) = worker.lock() else { continue };
            (
                guard.instance_id.clone(),
                guard.account_email.clone(),
                guard.slot_id.clone(),
                guard.retry_queue.len(),
                guard.session_id.clone(),
                guard.campaign_id.clone(),
            )
        };

        let connected = account_snapshot(&shared, &worker_instance)
            .map(|snapshot| !snapshot.tearing_down)
            .unwrap_or(false);
        let use_replacement = mode == "replacement" && !connected;
        if !connected && !use_replacement {
            return Err(
                "The associated browser instance is no longer connected. Refresh the instance list, then use Open New Instance & Retry Remaining."
                    .to_string(),
            );
        }

        if let Ok(mut guard) = worker.lock() {
            guard.status = "retrying".to_string();
        }
        emit_worker(
            &shared,
            &session_key,
            &campaign_key,
            worker,
            "retrying",
            details(json!({
                "recipient": Value::Null,
                "remaining": retry_count,
                "logs": if use_replacement {
                    format!("Opening a replacement browser instance for {retry_count} failed or unsent recipient(s).")
                } else {
                    format!("Retrying {retry_count} failed or unsent recipient(s) on the connected browser instance.")
                },
            })),
        );

        let retry_instance = if use_replacement {
            let profile = create_profile(
                &shared,
                json!({ "waitForLogin": false, "expectedEmail": worker_email }),
            )
            .await?;
            let new_id = profile
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if new_id.is_empty() {
                return Err("The replacement browser instance could not be prepared.".to_string());
            }
            created_instance_ids.push(new_id.clone());
            new_id
        } else {
            worker_instance
        };

        let queue = worker
            .lock()
            .map(|guard| guard.retry_queue.clone())
            .unwrap_or_default();
        let limit = queue.len();
        retry_recipients.extend(queue);
        retry_assignments.push(json!({
            "accountId": retry_instance,
            "instanceId": retry_instance,
            "slotId": slot_id,
            "limit": limit,
        }));
    }

    let mut merged = previous
        .payload
        .lock()
        .map(|payload| payload.as_object().cloned().unwrap_or_default())
        .unwrap_or_default();
    merged.insert("recipients".into(), Value::Array(retry_recipients));
    merged.insert("assignments".into(), Value::Array(retry_assignments));
    let result = start_run(shared.clone(), Value::Object(merged)).await?;

    for worker in &workers {
        let (session_key, campaign_key, retry_count) = {
            let Ok(mut guard) = worker.lock() else {
                continue;
            };
            let count = guard.retry_queue.len();
            guard.retry_queue.clear();
            guard.status = "retried".to_string();
            (guard.session_id.clone(), guard.campaign_id.clone(), count)
        };
        emit_worker(
            &shared,
            &session_key,
            &campaign_key,
            worker,
            "retried",
            details(json!({
                "recipient": Value::Null,
                "remaining": 0,
                "logs": format!("{retry_count} failed or unsent recipient(s) are retrying."),
            })),
        );
    }

    let mut result = result;
    if let Value::Object(map) = &mut result {
        map.insert("replacementInstanceIds".into(), json!(created_instance_ids));
    }
    Ok(result)
}

// --- reset / snapshots ---------------------------------------------------------

fn is_transmission_active(shared: &Shared) -> bool {
    shared
        .state
        .lock()
        .map(|state| state.reset_in_progress || !state.active_workers.is_empty())
        .unwrap_or(false)
}

async fn reset_all_instances(shared: Shared) -> Result<Value, String> {
    if shared
        .state
        .lock()
        .map(|state| state.reset_in_progress)
        .unwrap_or(false)
    {
        return Ok(json!({ "reset": true }));
    }
    if is_transmission_active(&shared) {
        return Err(
            "Instances are still sending data; wait for every transmission to finish.".to_string(),
        );
    }

    if let Ok(mut state) = shared.state.lock() {
        state.reset_in_progress = true;
    }

    let ids: Vec<String> = shared
        .state
        .lock()
        .map(|state| state.accounts.keys().cloned().collect())
        .unwrap_or_default();
    for id in ids {
        close_browser(&shared, &id, "browser-closed").await;
    }

    if let Ok(mut state) = shared.state.lock() {
        state.runs.clear();
        state.active_sessions.clear();
        state.run_order.clear();
        state.active_workers.clear();
        state.retry_locks.clear();
        state.accounts.clear();
        state.reset_in_progress = false;
    }
    Ok(json!({ "reset": true }))
}

fn list_profiles_value(shared: &Shared) -> Value {
    let Ok(state) = shared.state.lock() else {
        return json!([]);
    };
    Value::Array(
        state
            .accounts
            .values()
            .filter(|account| !account.tearing_down)
            .map(|account| {
                json!({
                    "id": account.id,
                    "email": account.email,
                    "name": account.name,
                    "ready": account.ready,
                    "processId": Value::Null,
                    "status": if account.ready { "ready" } else { "waiting-for-login" },
                    "pending": !account.ready,
                })
            })
            .collect(),
    )
}

fn list_runs_value(shared: &Shared) -> Value {
    let Ok(state) = shared.state.lock() else {
        return json!([]);
    };
    let mut runs = Vec::new();
    for session_id in &state.run_order {
        if !state.active_sessions.contains(session_id) {
            continue;
        }
        let Some(run) = state.runs.get(session_id) else {
            continue;
        };
        if let Ok(workers) = run.workers.lock() {
            for worker in workers.iter() {
                if let Ok(guard) = worker.lock() {
                    runs.push(json!({
                        "sessionId": run.session_id,
                        "campaignId": run.campaign_id,
                        "instanceId": guard.instance_id,
                        "accountId": guard.instance_id,
                        "workerId": guard.worker_id,
                        "slotId": guard.slot_id,
                        "status": guard.status,
                        "sentCount": guard.sent_count,
                        "failedCount": guard.failed_count,
                        "total": guard.total,
                        "currentIndex": guard.current_index,
                    }));
                }
            }
        }
    }
    Value::Array(runs)
}

fn transmission_state_value(shared: &Shared) -> Value {
    let (transmitting, active) = shared
        .state
        .lock()
        .map(|state| {
            (
                state.reset_in_progress || !state.active_workers.is_empty(),
                state.active_workers.len(),
            )
        })
        .unwrap_or((false, 0));
    json!({
        "transmitting": transmitting,
        "activeInstanceCount": active,
        "rendering": false,
        "inlineRendering": 0,
    })
}

// --- Commands ------------------------------------------------------------------

#[tauri::command]
pub async fn automation_add_profile(
    app: AppHandle,
    service: State<'_, AutomationService>,
    options: Option<Value>,
) -> Result<Value, String> {
    service.attach_app(&app);
    create_profile(&service.shared(), options.unwrap_or_else(|| json!({}))).await
}

#[tauri::command]
pub async fn automation_list_profiles(
    service: State<'_, AutomationService>,
) -> Result<Value, String> {
    Ok(list_profiles_value(&service.shared()))
}

#[tauri::command]
pub async fn automation_list_runs(service: State<'_, AutomationService>) -> Result<Value, String> {
    Ok(list_runs_value(&service.shared()))
}

#[tauri::command]
pub async fn automation_start(
    app: AppHandle,
    service: State<'_, AutomationService>,
    payload: Value,
) -> Result<Value, String> {
    service.attach_app(&app);
    start_run(service.shared(), payload).await
}

#[tauri::command]
pub async fn automation_retry(
    app: AppHandle,
    service: State<'_, AutomationService>,
    session_id: String,
    instance_id: Option<String>,
    mode: Option<String>,
) -> Result<Value, String> {
    service.attach_app(&app);
    retry(
        service.shared(),
        session_id,
        instance_id,
        mode.unwrap_or_else(|| "reuse".to_string()),
    )
    .await
}

#[tauri::command]
pub async fn automation_stop(
    service: State<'_, AutomationService>,
    instance_or_session_id: Value,
) -> Result<Value, String> {
    control_workers(&service.shared(), &instance_or_session_id, "stop")
}

#[tauri::command]
pub async fn automation_pause(
    service: State<'_, AutomationService>,
    instance_id: String,
) -> Result<Value, String> {
    control_workers(&service.shared(), &Value::String(instance_id), "pause")
}

#[tauri::command]
pub async fn automation_resume(
    service: State<'_, AutomationService>,
    instance_id: String,
) -> Result<Value, String> {
    control_workers(&service.shared(), &Value::String(instance_id), "resume")
}

#[tauri::command]
pub async fn automation_close_profile(
    service: State<'_, AutomationService>,
    instance_id: String,
) -> Result<Value, String> {
    let shared = service.shared();
    let exists = shared
        .state
        .lock()
        .map(|state| state.accounts.contains_key(&instance_id))
        .unwrap_or(false);
    if !exists {
        return Ok(json!({ "closed": false, "instanceId": instance_id }));
    }
    close_browser(&shared, &instance_id, "browser-closed").await;
    Ok(json!({ "closed": true, "instanceId": instance_id }))
}

#[tauri::command]
pub async fn automation_transmission_state(
    service: State<'_, AutomationService>,
) -> Result<Value, String> {
    Ok(transmission_state_value(&service.shared()))
}

#[tauri::command]
pub async fn automation_reset_all_instances(
    service: State<'_, AutomationService>,
) -> Result<Value, String> {
    reset_all_instances(service.shared()).await
}

#[tauri::command]
pub async fn app_reset_data(
    service: State<'_, AutomationService>,
    store: State<'_, Mutex<Store>>,
) -> Result<Value, String> {
    reset_all_instances(service.shared()).await?;

    let mut store = store.lock().map_err(|error| error.to_string())?;
    store.transact(|data| {
        data.insert("campaigns".to_string(), Value::Array(Vec::new()));
        data.remove("accounts");
        let account_keys: Vec<String> = data
            .keys()
            .filter(|key| key.starts_with("accounts:"))
            .cloned()
            .collect();
        for key in account_keys {
            data.remove(&key);
        }
        Ok(())
    })?;

    Ok(json!({ "reset": true }))
}
