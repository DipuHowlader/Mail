//! Headless automation self-test (Phase 4a).
//!
//! Proves the orchestration port end to end without a browser or a Gmail
//! account: a deterministic [`TestSender`] stands in for the (still deferred)
//! Gmail pipeline, and a [`super::CollectorSink`] captures the telemetry that
//! would otherwise go to the webview. Every command's underlying code path is
//! exercised — create/queues, start, pause, stop, retry-eligibility, list,
//! transmission state and reset.
//!
//! Run: `NOVAMAIL_SELFTEST_MODE=automation NOVAMAIL_WINDOW_HIDDEN=1 novamail.exe`
//!
//! The profile lifecycle (`addProfile`/`closeProfile`) drives a real headed
//! Chromium and is verified separately with
//! `NOVAMAIL_SELFTEST_MODE=profile`.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::browser::{BoxFuture, BrowserDriver, PageId, PdfOptions};

use super::mail_sender::{MailSender, SendFailure, SendReport, SendRequest};
use super::{AutomationService, CollectorSink, LoginState, Shared};

/// A driver that does nothing. The `TestSender` ignores it, so its only job is
/// to satisfy [`super::Account`].
struct FakeDriver;

impl BrowserDriver for FakeDriver {
    fn engine(&self) -> &'static str {
        "fake"
    }
    fn new_page<'a>(&'a self, _url: Option<&'a str>) -> BoxFuture<'a, Result<PageId, String>> {
        Box::pin(async { Ok(1) })
    }
    fn goto<'a>(&'a self, _page: PageId, _url: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn insert_text<'a>(
        &'a self,
        _page: PageId,
        _text: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn press_key<'a>(
        &'a self,
        _page: PageId,
        _key: crate::browser::Key,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn focus<'a>(&'a self, _page: PageId, _selector: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn click<'a>(&'a self, _page: PageId, _selector: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn set_file_input<'a>(
        &'a self,
        _page: PageId,
        _selector: &'a str,
        _files: Vec<std::path::PathBuf>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn wait_for_function<'a>(
        &'a self,
        _page: PageId,
        _script: &'a str,
        _timeout_ms: u64,
        _poll_ms: u64,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async { Ok(true) })
    }
    fn set_content<'a>(
        &'a self,
        _page: PageId,
        _html: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn evaluate<'a>(
        &'a self,
        _page: PageId,
        _script: &'a str,
    ) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async { Ok(json!(2)) })
    }
    fn screenshot<'a>(&'a self, _page: PageId) -> BoxFuture<'a, Result<Vec<u8>, String>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn print_pdf<'a>(
        &'a self,
        _page: PageId,
        _options: &'a PdfOptions,
    ) -> BoxFuture<'a, Result<Vec<u8>, String>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn close_page<'a>(&'a self, _page: PageId) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn shutdown<'a>(&'a self) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
}

/// Deterministic stand-in for the Gmail send step: recipients whose address
/// contains `fail` fail; everything else succeeds after `delay_ms`.
struct TestSender {
    delay_ms: u64,
}

impl MailSender for TestSender {
    fn send<'a>(&'a self, request: SendRequest) -> BoxFuture<'a, Result<SendReport, SendFailure>> {
        Box::pin(async move {
            let email = request
                .recipient
                .get("email")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if self.delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
            }
            if email.contains("fail") {
                Err(SendFailure::Recipient(format!(
                    "Simulated failure for {email}."
                )))
            } else {
                Ok(SendReport { subject: None })
            }
        })
    }
}

fn install_account(shared: &Shared, id: &str) {
    let mut state = shared.state.lock().expect("automation state");
    state.accounts.insert(
        id.to_string(),
        super::Account {
            id: id.to_string(),
            email: Some("sender@example.com".to_string()),
            name: "sender@example.com".to_string(),
            ready: true,
            expected_email: String::new(),
            driver: Arc::new(FakeDriver),
            page: 1,
            tearing_down: false,
            login: LoginState::Ready,
        },
    );
}

struct Harness {
    shared: Shared,
    sink: Arc<CollectorSink>,
}

fn harness(delay_ms: u64) -> Harness {
    let sink = Arc::new(CollectorSink::new());
    let service = AutomationService::build(Arc::new(TestSender { delay_ms }), Some(sink.clone()));
    Harness {
        shared: service.shared(),
        sink,
    }
}

fn events_with_status(events: &[Value], status: &str) -> usize {
    events
        .iter()
        .filter(|event| event.get("status").and_then(Value::as_str) == Some(status))
        .count()
}

pub async fn run() -> Value {
    let mut report = Map::new();
    let mut passed = true;

    // 1. A run that succeeds for two recipients and fails one.
    {
        let harness = harness(1);
        install_account(&harness.shared, "inst-a");
        let payload = json!({
            "campaignId": "camp-1",
            "recipients": [
                { "email": "alice@example.com" },
                { "email": "bob@example.com" },
                { "email": "fail@example.com" },
            ],
            "assignments": [{ "instanceId": "inst-a", "limit": 3 }],
            "subject": "Hello",
            "html": "<p>Hello</p>",
            "text": "Hello",
        });
        let result = super::start_run(harness.shared.clone(), payload).await;
        let ok = match &result {
            Ok(value) => {
                let sent = value.get("sentCount").and_then(Value::as_i64);
                let failed = value.get("failedCount").and_then(Value::as_i64);
                let total = value.get("total").and_then(Value::as_i64);
                let aborted = value.get("aborted").and_then(Value::as_bool);
                let failed_emails = value
                    .get("failedEmails")
                    .and_then(Value::as_array)
                    .map(Vec::len);
                sent == Some(2)
                    && failed == Some(1)
                    && total == Some(3)
                    && aborted == Some(false)
                    && failed_emails == Some(1)
            }
            Err(_) => false,
        };
        let events = harness.sink.snapshot();
        let telemetry_ok = events_with_status(&events, "sent") == 2
            && events_with_status(&events, "failed") == 1
            && events_with_status(&events, "completed") == 1;
        let session_matches = result
            .as_ref()
            .ok()
            .and_then(|value| value.get("sessionId").and_then(Value::as_str))
            .map(|session| {
                events
                    .iter()
                    .any(|event| event.get("sessionId").and_then(Value::as_str) == Some(session))
            })
            .unwrap_or(false);
        let entry_ok = ok && telemetry_ok && session_matches;
        passed &= entry_ok;
        report.insert(
            "run".into(),
            json!({
                "result": result.clone().unwrap_or(Value::Null),
                "sentEvents": events_with_status(&events, "sent"),
                "failedEvents": events_with_status(&events, "failed"),
                "completedEvents": events_with_status(&events, "completed"),
                "passed": entry_ok,
            }),
        );
    }

    // 2. Stop mid-run: the run ends aborted with unsent recipients left retryable.
    {
        let harness = harness(40);
        install_account(&harness.shared, "inst-b");
        let recipients: Vec<Value> = (0..5)
            .map(|i| json!({ "email": format!("user{i}@example.com") }))
            .collect();
        let payload = json!({
            "campaignId": "camp-2",
            "recipients": recipients,
            "assignments": [{ "instanceId": "inst-b", "limit": 5 }],
            "subject": "Hello",
            "html": "<p>Hello</p>",
        });
        let task_shared = harness.shared.clone();
        let handle = tauri::async_runtime::spawn(super::start_run(task_shared, payload));
        tokio::time::sleep(Duration::from_millis(70)).await;
        let control = super::control_workers(&harness.shared, &json!("inst-b"), "stop");
        let result = handle.await.expect("join").unwrap_or(Value::Null);
        let updated = control
            .as_ref()
            .ok()
            .and_then(|value| value.get("updated"))
            .and_then(Value::as_bool);
        let aborted = result.get("aborted").and_then(Value::as_bool);
        let sent_count = result.get("sentCount").and_then(Value::as_i64);
        let stopping_events = events_with_status(&harness.sink.snapshot(), "stopping");
        let entry_ok = updated == Some(true)
            && aborted == Some(true)
            && sent_count.unwrap_or(5) < 5
            && stopping_events >= 1;
        passed &= entry_ok;
        report.insert(
            "stop".into(),
            json!({
                "updated": updated,
                "stoppingEvents": stopping_events,
                "result": result,
                "passed": entry_ok,
            }),
        );
    }

    // 3. listRuns/transmissionState while a run is in flight, plus the reset guard.
    {
        let harness = harness(30);
        install_account(&harness.shared, "inst-c");
        let payload = json!({
            "campaignId": "camp-3",
            "recipients": [
                { "email": "one@example.com" },
                { "email": "two@example.com" },
            ],
            "assignments": [{ "instanceId": "inst-c", "limit": 2 }],
            "subject": "Hello",
            "html": "<p>Hello</p>",
        });
        let task_shared = harness.shared.clone();
        let handle = tauri::async_runtime::spawn(super::start_run(task_shared, payload));
        tokio::time::sleep(Duration::from_millis(40)).await;

        let runs = super::list_runs_value(&harness.shared);
        let listed = runs
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .any(|item| item.get("instanceId").and_then(Value::as_str) == Some("inst-c"))
            })
            .unwrap_or(false);
        let transmitting = super::transmission_state_value(&harness.shared)
            .get("transmitting")
            .and_then(Value::as_bool);
        let reset_blocked = super::reset_all_instances(harness.shared.clone())
            .await
            .is_err();

        super::control_workers(&harness.shared, &json!("inst-c"), "stop").ok();
        let _ = handle.await;

        let profiles_after = super::list_runs_value(&harness.shared);
        let idle = profiles_after
            .as_array()
            .map(Vec::is_empty)
            .unwrap_or(false);
        let idle_transmitting = super::transmission_state_value(&harness.shared)
            .get("transmitting")
            .and_then(Value::as_bool);

        let entry_ok = listed
            && transmitting == Some(true)
            && reset_blocked
            && idle
            && idle_transmitting == Some(false);
        passed &= entry_ok;
        report.insert(
            "lifecycle".into(),
            json!({
                "listedDuringRun": listed,
                "transmittingDuringRun": transmitting,
                "resetBlockedWhileSending": reset_blocked,
                "activeRunsAfterStop": profiles_after,
                "transmittingAfterStop": idle_transmitting,
                "passed": entry_ok,
            }),
        );
    }

    // 3b. Pause then resume keeps the queue intact.
    {
        let harness = harness(40);
        install_account(&harness.shared, "inst-p");
        let recipients: Vec<Value> = (0..3)
            .map(|i| json!({ "email": format!("p{i}@example.com") }))
            .collect();
        let payload = json!({
            "campaignId": "camp-p",
            "recipients": recipients,
            "assignments": [{ "instanceId": "inst-p", "limit": 3 }],
            "subject": "Hello",
            "html": "<p>Hello</p>",
        });
        let task_shared = harness.shared.clone();
        let handle = tauri::async_runtime::spawn(super::start_run(task_shared, payload));
        tokio::time::sleep(Duration::from_millis(50)).await;
        let paused = super::control_workers(&harness.shared, &json!("inst-p"), "pause");
        tokio::time::sleep(Duration::from_millis(50)).await;
        let resumed = super::control_workers(&harness.shared, &json!("inst-p"), "resume");
        let result = handle.await.expect("join").unwrap_or(Value::Null);
        let ok = paused
            .as_ref()
            .ok()
            .and_then(|value| value.get("updated"))
            .and_then(Value::as_bool)
            == Some(true)
            && resumed
                .as_ref()
                .ok()
                .and_then(|value| value.get("updated"))
                .and_then(Value::as_bool)
                == Some(true)
            && result.get("sentCount").and_then(Value::as_i64) == Some(3)
            && events_with_status(&harness.sink.snapshot(), "paused") >= 1;
        passed &= ok;
        report.insert(
            "pauseResume".into(),
            json!({
                "paused": paused.unwrap_or(Value::Null),
                "resumed": resumed.unwrap_or(Value::Null),
                "result": result,
                "pausedEvents": events_with_status(&harness.sink.snapshot(), "paused"),
                "passed": ok,
            }),
        );
    }

    // 4. Retry eligibility: a finished run with failures can be retried.
    {
        let harness = harness(1);
        install_account(&harness.shared, "inst-r");
        let payload = json!({
            "campaignId": "camp-r",
            "recipients": [
                { "email": "ok@example.com" },
                { "email": "fail@example.com" },
            ],
            "assignments": [{ "instanceId": "inst-r", "limit": 2 }],
            "subject": "Hello",
            "html": "<p>Hello</p>",
        });
        let result = super::start_run(harness.shared.clone(), payload)
            .await
            .unwrap_or(Value::Null);
        let session = result
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        // The retry re-runs the failed recipient; the account is still
        // connected, so it reuses the same instance and fails again.
        let retry_result = super::retry(harness.shared.clone(), session, None, "reuse".to_string())
            .await
            .unwrap_or(Value::Null);
        let failed_email_reported = result
            .get("failedEmails")
            .and_then(Value::as_array)
            .map(|items| {
                items.len() == 1
                    && items.iter().any(|item| {
                        item.get("email").and_then(Value::as_str) == Some("fail@example.com")
                    })
            })
            .unwrap_or(false);
        // The retry executed the queue a second time: 1 recipient, 1 failure.
        let ok = failed_email_reported
            && retry_result.get("failedCount").and_then(Value::as_i64) == Some(1)
            && retry_result.get("total").and_then(Value::as_i64) == Some(1);
        passed &= ok;
        report.insert(
            "retry".into(),
            json!({
                "firstResult": result,
                "retryResult": retry_result,
                "passed": ok,
            }),
        );
    }

    // 5. Profiles + reset.
    {
        let harness = harness(1);
        install_account(&harness.shared, "inst-z");
        let profiles = super::list_profiles_value(&harness.shared);
        let listed = profiles
            .as_array()
            .map(|items| {
                items.iter().any(|item| {
                    item.get("id").and_then(Value::as_str) == Some("inst-z")
                        && item.get("ready").and_then(Value::as_bool) == Some(true)
                })
            })
            .unwrap_or(false);
        let reset = super::reset_all_instances(harness.shared.clone()).await;
        let cleared = super::list_profiles_value(&harness.shared)
            .as_array()
            .map(Vec::is_empty)
            .unwrap_or(false);
        let ok = listed && reset.is_ok() && cleared;
        passed &= ok;
        report.insert(
            "profilesReset".into(),
            json!({
                "listed": listed,
                "reset": reset.unwrap_or(Value::Null),
                "profilesAfterReset": super::list_profiles_value(&harness.shared),
                "passed": ok,
            }),
        );
    }

    report.insert("passed".into(), json!(passed));
    Value::Object(report)
}
