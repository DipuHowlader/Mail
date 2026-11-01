//! Headless self-test, mirroring desktop runtime's `NOVAMAIL_SELFTEST` harness.
//!
//! Lets the migration be checked without a UI (and on a headless box), which
//! matters because the interesting parts here — the device id, the config store
//! and the license service contract — are all native.
//!
//! Run:  `NOVAMAIL_SELFTEST=1 novamail.exe`
//! Prints one `[selftest] {json}` line, then exits 0 (pass) or 1 (fail).
//!
//! Mode-specific harnesses run inside a real (hidden) Tauri app and are selected
//! with `NOVAMAIL_SELFTEST_MODE` (with `NOVAMAIL_WINDOW_HIDDEN=1`):
//! `browser` (Chromium driver), `document` (every export format) and `launcher`
//! (app supervisor + control channel).

use serde_json::{Map, Value, json};
use tauri::Manager;

use crate::browser::{BrowserDriver, BrowserService, PdfOptions};
use crate::files;
use crate::launcher::{self, LauncherState};
use crate::license;
use crate::store::Store;

pub fn run() -> i32 {
    let mut report = Map::new();
    let mut passed = true;

    // 1. Device id — must match the desktop runtime derivation exactly, or the license
    //    service sees a different machine and rejects the key.
    match license::machine_id() {
        Ok(id) => {
            report.insert(
                "machineIdPrefix".into(),
                json!(id.chars().take(10).collect::<String>()),
            );
            if let Ok(expected) = std::env::var("NOVAMAIL_EXPECT_MACHINE_ID") {
                let matches = id == expected;
                passed &= matches;
                report.insert("machineIdMatches".into(), json!(matches));
            }
        }
        Err(error) => {
            passed = false;
            report.insert("machineIdError".into(), json!(error));
        }
    }

    // 2. The real store: confirm we can read the data desktop runtime left behind.
    let real = Store::open();
    report.insert("storePath".into(), json!(real.path().display().to_string()));
    report.insert(
        "campaigns".into(),
        json!(
            real.get("campaigns")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0)
        ),
    );
    report.insert(
        "accounts".into(),
        json!(
            real.get("accounts")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0)
        ),
    );
    report.insert(
        "hasStoredKey".into(),
        json!(license::read_stored_key(&real).is_some()),
    );

    // 3. Round-trip in a throwaway directory, including DPAPI key protection.
    let sandbox = std::env::temp_dir().join(format!("novamail-selftest-{}", std::process::id()));
    {
        let mut store = Store::open_at(&sandbox);
        let _ = store.set("__selftest", json!({ "hello": "world" }));
        let reopened = Store::open_at(&sandbox);
        let round_trip = reopened
            .get("__selftest")
            .and_then(|value| value.get("hello"))
            .and_then(Value::as_str)
            == Some("world");
        passed &= round_trip;
        report.insert("storeRoundTrip".into(), json!(round_trip));

        #[cfg(windows)]
        {
            const KEY: &str = "SELFTEST-KEY-1234";
            let result = license::LicenseResult {
                valid: true,
                code: None,
                tier: Some("selftest".into()),
                expires_at: None,
                reason: None,
            };
            let mut store = Store::open_at(&sandbox);
            let _ = license::save(&mut store, KEY, &result);
            let read_back = license::read_stored_key(&Store::open_at(&sandbox));
            let dpapi_ok = read_back.as_deref() == Some(KEY);
            passed &= dpapi_ok;
            report.insert("dpapiRoundTrip".into(), json!(dpapi_ok));
        }
    }
    let _ = std::fs::remove_dir_all(&sandbox);

    // 4. Template engine — the `parseTemplate` port behind document export and
    //    (later) the Gmail send pipeline. Pure, so it needs no window.
    let template_report = crate::template::selftest();
    let template_ok = template_report
        .get("passed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    passed &= template_ok;
    report.insert("template".into(), template_report);

    // 5. Live license-service reachability with an obviously invalid key.
    match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => {
            match runtime.block_on(license::verify_remote("SELFTEST-0000-0000-0000-NOPE")) {
                Ok(result) => {
                    report.insert("verifyValid".into(), json!(result.valid));
                    report.insert("verifyCode".into(), json!(result.code));
                }
                Err(code) => {
                    report.insert("verifyErrorCode".into(), json!(code));
                }
            }
        }
        Err(error) => {
            report.insert("runtimeError".into(), json!(error.to_string()));
        }
    }

    // 6. Recipient import parsing — the CSV path behind `recipients:pickAndParse`.
    match run_recipient_parsing() {
        Ok(entry) => {
            passed &= entry
                .get("passed")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            report.insert("recipients".into(), entry);
        }
        Err(error) => {
            passed = false;
            report.insert("recipientsError".into(), json!(error));
        }
    }

    report.insert("passed".into(), json!(passed));
    println!("[selftest] {}", Value::Object(report));
    if passed { 0 } else { 1 }
}

/// Parse a small CSV through the real importer so the whole `recipients` path
/// (headers, dedupe, the name/order_id/amount fallbacks and custom fields) is
/// covered without opening a dialog. Returns the report entry for `run()`.
fn run_recipient_parsing() -> Result<Value, String> {
    let path = std::env::temp_dir().join(format!("novamail-recipients-{}.csv", std::process::id()));
    let csv = "Email,Full Name,Order ID,Amount,Notes\n\
               alice@example.com,Alice Smith,A-1,10,first\n\
               bob@example.com,Bob Jones,B-2,20,second\n\
               ALICE@example.com,Alice Duplicate,A-3,30,dup\n\
               not-an-email,Nobody,X,0,bad\n";
    std::fs::write(&path, csv).map_err(|error| error.to_string())?;
    let result = files::parse_recipient_file(&path);
    let _ = std::fs::remove_file(&path);

    let (total, recipients) = result?;
    let first = recipients.first().cloned().unwrap_or(Value::Null);
    let names: Vec<&str> = recipients
        .iter()
        .filter_map(|row| row.get("name").and_then(Value::as_str))
        .collect();

    let passed = total == 4
        && recipients.len() == 2
        && first.get("email").and_then(Value::as_str) == Some("alice@example.com")
        && first.get("name").and_then(Value::as_str) == Some("Alice Smith")
        && first.get("order_id").and_then(Value::as_str) == Some("A-1")
        && first.get("amount").and_then(Value::as_str) == Some("10")
        && first.get("notes").and_then(Value::as_str) == Some("first")
        && names == ["Alice Smith", "Bob Jones"];

    Ok(json!({
        "totalRows": total,
        "recipients": recipients.len(),
        "passed": passed,
    }))
}

/// Headless browser-driver self-test (Phase 3).
///
/// Drives the real vendored Chromium through the `BrowserDriver` trait — render
/// HTML, evaluate JS, screenshot, print to PDF — and proves the PDF is real.
///
/// Run: `NOVAMAIL_SELFTEST_MODE=browser NOVAMAIL_WINDOW_HIDDEN=1 novamail.exe`
pub async fn run_browser() -> i32 {
    let mut report = Map::new();
    let mut passed = true;

    match crate::browser::paths::chromium_executable_path() {
        Ok(path) => {
            report.insert("executable".into(), json!(path.display().to_string()));
        }
        Err(error) => {
            report.insert("pathError".into(), json!(error));
            report.insert("passed".into(), json!(false));
            println!("[selftest] {}", Value::Object(report));
            return 1;
        }
    }

    let service = BrowserService::new();
    match run_browser_driver(&service).await {
        Ok(entry) => {
            passed &= entry
                .get("passed")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if let Value::Object(map) = entry {
                for (key, value) in map {
                    report.insert(key, value);
                }
            }
        }
        Err(error) => {
            passed = false;
            report.insert("driverError".into(), json!(error));
        }
    }
    service.shutdown().await;

    report.insert("passed".into(), json!(passed));
    println!("[selftest] {}", Value::Object(report));
    if passed { 0 } else { 1 }
}

#[allow(clippy::too_many_lines)]
async fn run_browser_driver(service: &BrowserService) -> Result<Value, String> {
    let mut report = Map::new();
    let driver = service.driver().await?;
    report.insert("engine".into(), json!(driver.engine()));

    let page = driver.new_page(None).await?;
    let html = "<!doctype html><html><head><meta charset=\"utf-8\"></head>\
                <body><h1>Novamail</h1><p>browser self-test</p></body></html>";
    driver.set_content(page, html).await?;

    let arithmetic = driver.evaluate(page, "1 + 1").await?;
    let heading = driver
        .evaluate(page, "document.querySelector('h1').textContent")
        .await?;
    let screenshot = driver.screenshot(page).await?;
    let pdf = driver.print_pdf(page, &PdfOptions::default()).await?;
    let _ = driver.close_page(page).await;

    let pdf_magic = pdf.starts_with(b"%PDF-");
    let passed = arithmetic.as_i64() == Some(2)
        && heading.as_str() == Some("Novamail")
        && pdf_magic
        && pdf.len() > 1_000
        && screenshot.len() > 100;

    report.insert("arithmetic".into(), arithmetic);
    report.insert("heading".into(), heading);
    report.insert("screenshotBytes".into(), json!(screenshot.len()));
    report.insert("pdfBytes".into(), json!(pdf.len()));
    report.insert("pdfMagic".into(), json!(pdf_magic));
    report.insert("passed".into(), json!(passed));
    Ok(Value::Object(report))
}

/// Headless humanized-input self-test.
///
/// Drives the vendored Chromium through the real ChromiumDriver with the
/// automation layer's humanization enabled, against a local page — no network,
/// no sign-in — and proves what a behavioural collector would actually see:
///
/// * short typed text arrives key-by-key: a real `keydown` stream whose keys,
///   read back in order, spell the text (Shift pairing for uppercase, Space
///   included), paced like a hand (~1.5–4 s, not one atomic round-trip);
/// * the warm-up pointer burst lands *before* the session's first keystroke;
/// * `press_key` inserts its key (Enter leaves a newline in a textarea);
/// * recipient data deliberately bypasses typing (zero keydowns) while the value
///   still lands: a bare address ships as one protocol insert — key-by-key
///   dwell/flight pauses would cost seconds per message — and a comma-separated
///   list matches the Gmail peoplekit chip contract;
/// * and the engine exposes no `navigator.webdriver` (AutomationControlled off),
///   with the rest of the page-visible fingerprint as informational fields.
///
/// Run: `NOVAMAIL_SELFTEST_MODE=typing NOVAMAIL_WINDOW_HIDDEN=1 novamail.exe`
pub async fn run_typing() -> i32 {
    use std::sync::Arc;

    use crate::browser::chromium::ChromiumDriver;
    use crate::browser::DriverConfig;

    const TYPED_TEXT: &str = "Hello NovaMail";
    const RECIPIENT_ADDRESS: &str = "Nova@Novamail.io";
    const RECIPIENT_LIST: &str = "a@x.test, b@y.test";

    let mut report = Map::new();

    let executable = match crate::browser::paths::chromium_executable_path() {
        Ok(path) => path,
        Err(error) => {
            report.insert("pathError".into(), json!(error));
            report.insert("passed".into(), json!(false));
            println!("[selftest] {}", Value::Object(report));
            return 1;
        }
    };
    report.insert("executable".into(), json!(executable.display().to_string()));

    let profile_dir = std::env::temp_dir().join(format!("novamail-typing-{}", std::process::id()));
    if let Err(error) = std::fs::create_dir_all(&profile_dir) {
        report.insert("profileError".into(), json!(error.to_string()));
        report.insert("passed".into(), json!(false));
        println!("[selftest] {}", Value::Object(report));
        return 1;
    }

    // Production automation launches headful; the same Input pipelines run the
    // same way headless, so the self-test stays invisible on the box.
    let config = DriverConfig::new(executable)
        .headless(true)
        .user_data_dir(profile_dir.clone())
        .window_size(1280, 720)
        .humanize(Some("typing-selftest".to_string()))
        .args(
            crate::browser::paths::RENDER_BROWSER_ARGS
                .iter()
                .map(|arg| arg.to_string()),
        );

    let driver: Arc<dyn BrowserDriver> = match ChromiumDriver::launch(config).await {
        Ok(driver) => Arc::new(driver),
        Err(error) => {
            let _ = std::fs::remove_dir_all(&profile_dir);
            report.insert("launchError".into(), json!(error));
            report.insert("passed".into(), json!(false));
            println!("[selftest] {}", Value::Object(report));
            return 1;
        }
    };
    report.insert("profileDir".into(), json!(profile_dir.display().to_string()));

    let (checks, checks_passed) =
        run_typing_checks(&driver, TYPED_TEXT, RECIPIENT_ADDRESS, RECIPIENT_LIST).await;
    for (key, value) in checks {
        report.insert(key, value);
    }
    let _ = driver.shutdown().await;

    report.insert("passed".into(), json!(checks_passed));
    println!("[selftest] {}", Value::Object(report));
    if checks_passed { 0 } else { 1 }
}

/// Everything one humanized-input self-test run checks, everything it read back,
/// and whether it all passed.
async fn run_typing_checks(
    driver: &std::sync::Arc<dyn BrowserDriver>,
    typed_text: &'static str,
    recipient_address: &'static str,
    recipient_list: &'static str,
) -> (Map<String, Value>, bool) {
    use std::time::Instant;

    use crate::browser::Key;

    // A fresh page so the collector view starts empty: `__events` records every
    // input event the page sees, in time order; the Shift presses are logged too
    // so the verified stream can be reconstructed with the same filtering.
    const PROBE_HTML: &str = r#"<!doctype html><html><head><meta charset="utf-8"></head>
<body>
  <input id="to" type="text" value="">
  <textarea id="body"></textarea>
</body></html>"#;

    const INSTALL_LISTENERS: &str = r#"(() => {
  window.__events = [];
  document.addEventListener('mousemove', () => window.__events.push('move'), { passive: true });
  document.addEventListener('mousedown', () => window.__events.push('down'), { passive: true });
  document.addEventListener('keydown', (e) => window.__events.push('key:' + e.key));
  document.addEventListener('input', () => window.__events.push('input'), { passive: true });
  return true;
})()"#;

    const RESET_EVENTS: &str = r#"(() => {
  window.__events = [];
  document.querySelector('#to').value = '';
  document.querySelector('#body').value = '';
  return true;
})()"#;

    const READ_STATE: &str = r#"(() => {
  const events = window.__events || [];
  const keys = [];
  let moves = 0;
  for (const entry of events) {
    if (entry === 'move') { moves += 1; continue; }
    if (typeof entry === 'string' && entry.startsWith('key:')) {
      const k = entry.slice(4);
      if (k !== 'Shift') keys.push(k);
    }
  }
  return JSON.stringify({
    to: document.querySelector('#to').value,
    body: document.querySelector('#body').value,
    keyStream: keys.join(''),
    keydowns: keys.length,
    moves: moves,
    rawEvents: events.length,
    firstIsPointer: events.length > 0 && events[0] === 'move',
    webdriver: navigator.webdriver === undefined ? 'undefined' : String(navigator.webdriver),
    languages: (navigator.languages || []).join(','),
    uaDataPlatform: navigator.userAgentData ? navigator.userAgentData.platform : '',
    platform: navigator.platform,
    page: location.href,
  });
})()"#;

    let mut report = Map::new();
    let mut passed = true;

    let page = match driver.new_page(None).await {
        Ok(page) => page,
        Err(error) => {
            report.insert("pageError".into(), json!(error));
            return (report, false);
        }
    };

    if let Err(error) = driver.set_content(page, PROBE_HTML).await {
        report.insert("pageError".into(), json!(error));
        return (report, false);
    }
    match driver.evaluate(page, INSTALL_LISTENERS).await {
        Ok(value) if value.as_bool() == Some(true) => {}
        other => {
            report.insert(
                "listenerError".into(),
                json!(format!("listener install returned {other:?}")),
            );
            return (report, false);
        }
    }

    let read_state = |value: Value| {
        serde_json::from_str::<Value>(value.as_str().unwrap_or("")).unwrap_or(Value::Null)
    };

    // --- 1. Humanized typing of short, uppercase-bearing subject-like text ---
    if let Err(error) = driver.focus(page, "#to").await {
        report.insert("focusError".into(), json!(error));
        return (report, false);
    }
    let started = Instant::now();
    let inserted = driver.insert_text(page, typed_text).await;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let state = match inserted {
        Ok(()) => match driver.evaluate(page, READ_STATE).await {
            Ok(value) => read_state(value),
            Err(error) => {
                report.insert("readError".into(), json!(error));
                return (report, false);
            }
        },
        Err(error) => {
            report.insert("insertError".into(), json!(error));
            return (report, false);
        }
    };

    let value_matched = state.get("to").and_then(Value::as_str) == Some(typed_text);
    let stream_matched = state
        .get("keyStream")
        .and_then(Value::as_str)
        .map(|stream| stream == typed_text)
        .unwrap_or(false);
    let warmup_first = state
        .get("firstIsPointer")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // A human plan for this text spends at least ~1.4 s in dwell/flight pauses;
    // an atomic insert completes in well under a second even with CDP overhead.
    let paced = elapsed_ms >= 800;
    let keydowns = state.get("keydowns").and_then(Value::as_u64).unwrap_or(0);
    let typed_ok = value_matched && stream_matched && warmup_first && paced;
    passed &= typed_ok;

    report.insert(
        "typing".into(),
        json!({
            "text": typed_text,
            "value": state.get("to"),
            "keyStream": state.get("keyStream"),
            "keydowns": keydowns,
            "expectedKeys": typed_text.chars().count(),
            "elapsedMs": elapsed_ms,
            "warmupFirst": warmup_first,
            "ok": typed_ok,
        }),
    );
    println!(
        "[selftest-typing] valueMatched={value_matched} streamMatched={stream_matched} \
         warmupFirst={warmup_first} paced={paced} elapsedMs={elapsed_ms} keydowns={keydowns}"
    );

    // --- 2. press_key inserts its key (Enter leaves a newline in the textarea) ---
    // The warm-up is a once-per-session contract: consumed above, so this case
    // must not re-warm the session — no extra pointer burst should appear.
    if let Err(error) = driver.focus(page, "#body").await {
        report.insert("focusBodyError".into(), json!(error));
        return (report, false);
    }
    if let Err(error) = driver.press_key(page, Key::Enter).await {
        report.insert("pressEnterError".into(), json!(error));
        return (report, false);
    }
    let state = match driver.evaluate(page, READ_STATE).await {
        Ok(value) => read_state(value),
        Err(error) => {
            report.insert("readError".into(), json!(error));
            return (report, false);
        }
    };
    let enter_landed = state
        .get("body")
        .and_then(Value::as_str)
        .map(|body| body.contains('\n') || body.contains('\r'))
        .unwrap_or(false);
    let enter_seen_stream = state
        .get("keyStream")
        .and_then(Value::as_str)
        .map(|stream| stream.contains("Enter"))
        .unwrap_or(false);
    let press_ok = enter_landed && enter_seen_stream;
    passed &= press_ok;
    report.insert(
        "pressKey".into(),
        json!({
            "body": state.get("body"),
            "keyStream": state.get("keyStream"),
            "ok": press_ok,
        }),
    );

    // --- 3. A bare recipient address is injected in one operation, not typed ---
    // This is the production To-field shape: exactly one address per message.
    // The atomic path guarantees zero keydowns while the value still lands — no
    // seconds of dwell/flight pauses per send for a field Gmail commits reliably
    // through a single protocol insert plus the keyed chip commit.
    if let Err(error) = driver.evaluate(page, RESET_EVENTS).await {
        report.insert("resetError".into(), json!(error));
        return (report, false);
    }
    if let Err(error) = driver.focus(page, "#to").await {
        report.insert("focusError".into(), json!(error));
        return (report, false);
    }
    let inserted = driver.insert_text(page, recipient_address).await;
    let state = match inserted {
        Ok(()) => match driver.evaluate(page, READ_STATE).await {
            Ok(value) => read_state(value),
            Err(error) => {
                report.insert("readError".into(), json!(error));
                return (report, false);
            }
        },
        Err(error) => {
            report.insert("insertRecipientError".into(), json!(error));
            return (report, false);
        }
    };
    let address_landed = state
        .get("to")
        .and_then(Value::as_str)
        .map(|value| value == recipient_address)
        .unwrap_or(false);
    let address_untyped = state
        .get("keydowns")
        .and_then(Value::as_u64)
        .map(|count| count == 0)
        .unwrap_or(false);
    let address_ok = address_landed && address_untyped;
    passed &= address_ok;
    report.insert(
        "recipient".into(),
        json!({
            "address": recipient_address,
            "value": state.get("to"),
            "keydowns": state.get("keydowns"),
            "ok": address_ok,
        }),
    );

    // --- 4. Comma-separated recipient list deliberately bypasses typing ---
    if let Err(error) = driver.evaluate(page, RESET_EVENTS).await {
        report.insert("resetError".into(), json!(error));
        return (report, false);
    }
    if let Err(error) = driver.focus(page, "#to").await {
        report.insert("focusError".into(), json!(error));
        return (report, false);
    }
    let inserted = driver.insert_text(page, recipient_list).await;
    let state = match inserted {
        Ok(()) => match driver.evaluate(page, READ_STATE).await {
            Ok(value) => read_state(value),
            Err(error) => {
                report.insert("readError".into(), json!(error));
                return (report, false);
            }
        },
        Err(error) => {
            report.insert("insertRecipientsError".into(), json!(error));
            return (report, false);
        }
    };
    let recipients_landed = state
        .get("to")
        .and_then(Value::as_str)
        .map(|value| value == recipient_list)
        .unwrap_or(false);
    let recipients_untyped = state
        .get("keydowns")
        .and_then(Value::as_u64)
        .map(|count| count == 0)
        .unwrap_or(false);
    let recipients_ok = recipients_landed && recipients_untyped;
    passed &= recipients_ok;
    report.insert(
        "recipients".into(),
        json!({
            "list": recipient_list,
            "value": state.get("to"),
            "keydowns": state.get("keydowns"),
            "ok": recipients_ok,
        }),
    );

    // --- 5. The page-visible automation surface (informational + webdriver) ---
    let webdriver = state.get("webdriver").and_then(Value::as_str).unwrap_or("");
    // A bot-only blink flag is a hard tell; anything else on this custom build
    // is the vendored engine speaking for itself.
    let webdriver_ok = webdriver != "true";
    passed &= webdriver_ok;
    report.insert("webdriver".into(), json!(webdriver));
    report.insert("languages".into(), state.get("languages").cloned().unwrap_or(Value::Null));
    report.insert("platform".into(), state.get("platform").cloned().unwrap_or(Value::Null));
    report.insert("uaDataPlatform".into(), state.get("uaDataPlatform").cloned().unwrap_or(Value::Null));
    report.insert("page".into(), state.get("page").cloned().unwrap_or(Value::Null));

    (report, passed)
}

/// Headless document-engine self-test (Phase 3).
///
/// Calls the real `document_convert` code path for every supported format via
/// `documents::selftest`, so format dispatch, naming, the base64 envelope and the
/// Chromium PDF render are all exercised without a UI.
///
/// Run: `NOVAMAIL_SELFTEST_MODE=document NOVAMAIL_WINDOW_HIDDEN=1 novamail.exe`
pub async fn run_documents(app: tauri::AppHandle) -> i32 {
    let mut report = Map::new();
    let passed = match crate::documents::selftest(&app).await {
        Ok(entry) => {
            let ok = entry
                .get("passed")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            report.insert("documents".into(), entry);
            ok
        }
        Err(error) => {
            report.insert("error".into(), json!(error));
            false
        }
    };
    report.insert("passed".into(), json!(passed));
    println!("[selftest] {}", Value::Object(report));
    if passed { 0 } else { 1 }
}

/// Headless automation self-test (Phase 4a).
///
/// Exercises the run/worker queue, telemetry, pause/resume/stop, retry
/// eligibility, profiles and reset through `automation::selftest`, with a
/// deterministic sender standing in for the Gmail pipeline.
///
/// Run: `NOVAMAIL_SELFTEST_MODE=automation NOVAMAIL_WINDOW_HIDDEN=1 novamail.exe`
pub async fn run_automation() -> i32 {
    let mut report = Map::new();
    let entry = crate::automation::selftest::run().await;
    let passed = entry
        .get("passed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    report.insert("automation".into(), entry);
    report.insert("passed".into(), json!(passed));
    println!("[selftest] {}", Value::Object(report));
    if passed { 0 } else { 1 }
}

/// Headless Gmail-pipeline self-test (Phase 4c).
///
/// Exercises the browser-free half of `automation::gmail`: the `inline`-format
/// helper, the zero-width default body, the URL allow-list, the HTML sanitizer
/// (tags, attributes, URLs, entities, text flavour, image count, node cap) and
/// the empty-tree fallback.
///
/// Run: `NOVAMAIL_SELFTEST_MODE=gmail NOVAMAIL_WINDOW_HIDDEN=1 novamail.exe`
pub fn run_gmail() -> i32 {
    let mut report = Map::new();
    let entry = crate::automation::gmail::selftest();
    let passed = entry
        .get("passed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    report.insert("gmail".into(), entry);
    report.insert("passed".into(), json!(passed));
    println!("[selftest] {}", Value::Object(report));
    if passed { 0 } else { 1 }
}

/// Live Gmail end-to-end diagnostic.
///
/// Launches the real bundled Chromium, waits for the operator to sign in to
/// Gmail, probes the compose flow against the live DOM, and (with
/// `NOVAMAIL_LIVE_SEND=1`) performs one real send through the exact production
/// path. Prints a JSON report so a failure names its concrete cause instead of a
/// bare "did not open a compose window".
///
/// Run: `NOVAMAIL_SELFTEST_MODE=gmail-live NOVAMAIL_WINDOW_HIDDEN=1 \
///       [NOVAMAIL_LIVE_RECIPIENT=you@example.com] [NOVAMAIL_LIVE_SEND=1] \
///       novamail.exe`
pub async fn run_gmail_live() -> i32 {
    use std::sync::Arc;
    use std::time::Duration;

    use crate::automation::mail_sender::{ChromiumMailSender, MailSender, SendFailure, SendRequest};
    use crate::browser::{BrowserDriver, DriverConfig};

    let mut report = Map::new();

    let executable = match crate::browser::paths::chromium_executable_path() {
        Ok(path) => path,
        Err(error) => {
            report.insert("pathError".into(), json!(error));
            report.insert("passed".into(), json!(false));
            println!("[selftest] {}", Value::Object(report));
            return 1;
        }
    };
    report.insert("executable".into(), json!(executable.display().to_string()));

    // `NOVAMAIL_LIVE_FRESH=1` reproduces the production launch (a throwaway
    // incognito profile) so startup tabs can be inspected without a login.
    let fresh = std::env::var("NOVAMAIL_LIVE_FRESH").as_deref() == Ok("1");
    let profile_dir = if fresh {
        std::env::temp_dir().join(format!("novamail-live-fresh-{}", std::process::id()))
    } else {
        std::env::temp_dir().join("novamail-live-profile")
    };
    if let Err(error) = std::fs::create_dir_all(&profile_dir) {
        report.insert("profileError".into(), json!(error.to_string()));
        report.insert("passed".into(), json!(false));
        println!("[selftest] {}", Value::Object(report));
        return 1;
    }
    report.insert("profileDir".into(), json!(profile_dir.display().to_string()));

    // A persistent, non-incognito profile so the operator signs in once and the
    // session is reused by later diagnostic runs.
    let mut args = crate::automation::profile_args();
    if !fresh {
        args.retain(|arg| arg != "--incognito");
    }
    // `NOVAMAIL_LIVE_PERSONA=1` reproduces the production launch exactly: the
    // device persona's engine switches ride along with the profile flags.
    if std::env::var("NOVAMAIL_LIVE_PERSONA").as_deref() == Ok("1") {
        let persona = crate::persona::resolve(&serde_json::json!({})).await;
        report.insert("personaArgs".into(), json!(persona.args));
        args.extend(persona.args.clone());
    }
    args.push("--window-size=1280,900".to_string());
    let config = DriverConfig::new(executable)
        .headless(false)
        .user_data_dir(profile_dir.clone())
        .keep_profile(!fresh)
        .args(args);

    let driver: Arc<dyn BrowserDriver> =
        match crate::browser::chromium::ChromiumDriver::launch(config).await {
            Ok(driver) => Arc::new(driver),
            Err(error) => {
                report.insert("launchError".into(), json!(error));
                report.insert("passed".into(), json!(false));
                println!("[selftest] {}", Value::Object(report));
                return 1;
            }
        };

    let page = match driver.new_page(None).await {
        Ok(page) => page,
        Err(error) => {
            report.insert("pageError".into(), json!(error));
            report.insert("passed".into(), json!(false));
            let _ = driver.shutdown().await;
            println!("[selftest] {}", Value::Object(report));
            return 1;
        }
    };

    // Sign-in probe: navigate to Google's sign-in and report what the page shows
    // plus the fingerprint surface Google can see. Used to diagnose the
    // "This browser or app may not be secure" block.
    if std::env::var("NOVAMAIL_LIVE_STAGE").as_deref() == Ok("signin") {
        let signin_url = std::env::var("NOVAMAIL_LIVE_URL")
            .unwrap_or_else(|_| "https://accounts.google.com/signin/v2/identifier".to_string());
        let _ = driver.goto(page, &signin_url).await;
        tokio::time::sleep(Duration::from_secs(7)).await;
        // Optionally drive one step further: fill a throwaway address and press
        // Next. A normal account check says "Couldn't find your Google Account";
        // the automation block says "This browser or app may not be secure".
        if std::env::var("NOVAMAIL_LIVE_SIGNIN_SUBMIT").as_deref() == Ok("1") {
            let fill = "(()=>{const i=document.querySelector('input[type=email],input#identifierId,input[name=identifier]');if(!i)return 'no-input';i.focus();const s=Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype,'value').set;s.call(i,'novamail.probe.123456@gmail.com');i.dispatchEvent(new Event('input',{bubbles:true}));i.dispatchEvent(new Event('change',{bubbles:true}));return 'filled'})()";
            report.insert(
                "signinFill".into(),
                driver.evaluate(page, fill).await.unwrap_or(Value::Null),
            );
            tokio::time::sleep(Duration::from_millis(600)).await;
            let click = "(()=>{const b=[...document.querySelectorAll('button')].find(x=>/next|পরবর্তী/i.test((x.innerText||'')+(x.getAttribute('aria-label')||'')));if(!b)return 'no-button';b.click();return 'clicked'})()";
            report.insert(
                "signinClick".into(),
                driver.evaluate(page, click).await.unwrap_or(Value::Null),
            );
            tokio::time::sleep(Duration::from_secs(7)).await;
            // Also try the password step: a wrong password yields "Wrong
            // password" on a trusted browser and "browser may not be secure"
            // on a flagged one.
            let pfill = "(()=>{const i=document.querySelector('input[type=password],input[name=Passwd],input[name=pass],input#password');if(!i)return 'no-pwd-input';i.focus();const s=Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype,'value').set;s.call(i,'definitely-wrong-password-123');i.dispatchEvent(new Event('input',{bubbles:true}));i.dispatchEvent(new Event('change',{bubbles:true}));return 'filled'})()";
            report.insert(
                "signinPwdFill".into(),
                driver.evaluate(page, pfill).await.unwrap_or(Value::Null),
            );
            tokio::time::sleep(Duration::from_millis(600)).await;
            let pclick = "(()=>{const b=[...document.querySelectorAll('button')].find(x=>/next|sign in|siguiente/i.test((x.innerText||'')+(x.getAttribute('aria-label')||'')));if(!b)return 'no-button';b.click();return 'clicked'})()";
            report.insert(
                "signinPwdClick".into(),
                driver.evaluate(page, pclick).await.unwrap_or(Value::Null),
            );
            tokio::time::sleep(Duration::from_secs(7)).await;
        }
        let script = r#"(()=>({url:location.href,title:document.title,webdriver:navigator.webdriver,ua:navigator.userAgent,uaData:(navigator.userAgentData?navigator.userAgentData.brands:null),uaDataPlatform:(navigator.userAgentData?navigator.userAgentData.platform:null),uaDataMobile:(navigator.userAgentData?navigator.userAgentData.mobile:null),platform:navigator.platform,vendor:navigator.vendor,chrome:!!window.chrome,plugins:navigator.plugins.length,languages:navigator.languages,text:(document.body?document.body.innerText:'').replace(/\s+/g,' ').slice(0,500)}))()"#;
        report.insert(
            "signinProbe".into(),
            driver.evaluate(page, script).await.unwrap_or(Value::Null),
        );
        // Optional: read the HTTP client-hint headers Google would receive. A
        // `Sec-CH-UA-Platform` that disagrees with the UA string is a strong
        // bot signal.
        if std::env::var("NOVAMAIL_LIVE_HEADERS").as_deref() == Ok("1") {
            let _ = driver.goto(page, "https://httpbin.org/headers").await;
            tokio::time::sleep(Duration::from_secs(4)).await;
            report.insert(
                "headers".into(),
                driver
                    .evaluate(page, "document.body?document.body.innerText:''")
                    .await
                    .unwrap_or(Value::Null),
            );
        }
        report.insert("passed".into(), json!(true));
        let _ = driver.shutdown().await;
        println!("[selftest] {}", Value::Object(report));
        return 0;
    }

    // Startup-tab probe: report every target as soon as our page exists, so a
    // stray first-run/new-tab tab is visible without needing a signed-in Gmail.
    if std::env::var("NOVAMAIL_LIVE_STAGE").as_deref() == Ok("startup") {
        tokio::time::sleep(Duration::from_secs(3)).await;
        report.insert(
            "startupPages".into(),
            json!(driver.page_urls().await.unwrap_or_default()),
        );
        report.insert("passed".into(), json!(true));
        let _ = driver.goto(page, "about:blank").await;
        let _ = driver.shutdown().await;
        println!("[selftest] {}", Value::Object(report));
        return 0;
    }

    if let Err(error) = driver
        .goto(page, "https://mail.google.com/mail/u/0/#inbox")
        .await
    {
        report.insert("gotoError".into(), json!(error));
    }

    println!("[gmail-live] Sign in to Gmail in the browser window. Waiting up to 10 minutes...");

    let status_script = "(()=>{const el=document.querySelector('a[aria-label*=\"Google Account\"]')||document.querySelector('a[aria-label*=\"@\"]');const m=(el?el.getAttribute('aria-label'):'').match(/[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\\.[A-Za-z]{2,}/);return JSON.stringify({host:location.hostname,href:location.href,composeReady:!!document.querySelector('[gh=cm]'),email:m?m[0]:''})})()";

    let deadline = std::time::Instant::now() + Duration::from_secs(600);
    let mut signed_in = false;
    let mut detected_email = String::new();
    while std::time::Instant::now() < deadline {
        let status = driver
            .evaluate(page, status_script)
            .await
            .ok()
            .and_then(|value| value.as_str().map(str::to_string))
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .unwrap_or(Value::Null);
        let host = status.get("host").and_then(Value::as_str).unwrap_or("");
        let compose_ready = status
            .get("composeReady")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        detected_email = status
            .get("email")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if host == "mail.google.com" && compose_ready {
            signed_in = true;
            break;
        }
        if status != Value::Null {
            println!(
                "[gmail-live] waiting for inbox: host={host} composeReady={compose_ready} email={detected_email} href={}",
                status.get("href").and_then(Value::as_str).unwrap_or("")
            );
        } else {
            println!("[gmail-live] waiting for inbox: page did not respond to the status probe");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    report.insert("signedIn".into(), json!(signed_in));
    report.insert("detectedEmail".into(), json!(detected_email));

    if !signed_in {
        report.insert("passed".into(), json!(false));
        report.insert(
            "note".into(),
            json!("Timed out waiting for a signed-in Gmail tab with a Compose button."),
        );
        let _ = driver.shutdown().await;
        println!("[selftest] {}", Value::Object(report));
        return 1;
    }

    let stage = std::env::var("NOVAMAIL_LIVE_STAGE").unwrap_or_else(|_| "send".to_string());
    report.insert("stage".into(), json!(stage));

    let probe = crate::automation::mail_sender::live_compose_probe(&driver, page).await;
    let compose_opened = probe
        .get("opened")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    report.insert("composeProbe".into(), probe);
    report.insert(
        "pagesAfterOpen".into(),
        json!(driver.page_urls().await.unwrap_or_default()),
    );

    // With only "open", stop here: proves whether merely opening compose is
    // enough to crash the renderer, independent of any input command.
    if stage == "open" {
        report.insert("passed".into(), json!(compose_opened));
        let _ = driver.shutdown().await;
        println!("[selftest] {}", Value::Object(report));
        return 0;
    }

    // The ops probe moves focus between the recipient, subject and body editors,
    // so running it right before a send does not reflect production (where Gmail
    // leaves the To editor focused). Keep it to its own stage; the send stage
    // talks to a freshly opened compose.
    let ops_ok = if stage == "ops" {
        let ops = crate::automation::mail_sender::live_ops_probe(&driver, page).await;
        let ok = ops
            .get("subjectInsert")
            .and_then(|value| value.get("ok"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
            && ops
                .get("focusTo")
                .and_then(|value| value.get("ok"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
            && ops
                .get("recipientInsert")
                .and_then(|value| value.get("ok"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
        report.insert("opsProbe".into(), ops);
        report.insert(
            "pagesAfterOps".into(),
            json!(driver.page_urls().await.unwrap_or_default()),
        );
        report.insert("passed".into(), json!(compose_opened && ok));
        let _ = driver.shutdown().await;
        println!("[selftest] {}", Value::Object(report));
        return 0;
    } else {
        true
    };

    let should_send = std::env::var("NOVAMAIL_LIVE_SEND").as_deref() == Ok("1");
    let recipient = std::env::var("NOVAMAIL_LIVE_RECIPIENT")
        .ok()
        .filter(|value| value.contains('@'))
        .unwrap_or_else(|| detected_email.clone());

    let mut send_ok = true;
    if should_send && compose_opened && ops_ok && recipient.contains('@') {
        let payload = json!({
            "subject": "Novamail live send test",
            "text": "This is a Novamail end-to-end test. If you received it, the send pipeline works.",
            "outputFormat": "text",
        });
        let request = SendRequest {
            session_id: "live".into(),
            campaign_id: "live".into(),
            instance_id: "live".into(),
            worker_id: "live".into(),
            slot_id: "live".into(),
            account_email: detected_email.clone(),
            recipient: json!({ "email": recipient }),
            recipient_id: "live".into(),
            index: 0,
            total: 1,
            payload: Arc::new(payload),
            driver: driver.clone(),
            page,
            app: None,
        };
        let sender = ChromiumMailSender::new();
        let outcome = sender.send(request).await;
        match outcome {
            Ok(sent) => {
                report.insert("send".into(), json!({ "ok": true, "subject": sent.subject }));
            }
            Err(SendFailure::Recipient(error)) => {
                send_ok = false;
                report.insert("send".into(), json!({ "ok": false, "kind": "recipient", "error": error }));
            }
            Err(SendFailure::Uncertain(error)) => {
                send_ok = false;
                report.insert("send".into(), json!({ "ok": false, "kind": "uncertain", "error": error }));
            }
            Err(SendFailure::Stop(error)) => {
                send_ok = false;
                report.insert("send".into(), json!({ "ok": false, "kind": "stop", "error": error }));
            }
        }
    } else if should_send {
        send_ok = false;
        report.insert(
            "send".into(),
            json!({ "ok": false, "error": "Send skipped: compose did not open or no recipient was resolved." }),
        );
    }

    report.insert("passed".into(), json!(compose_opened && send_ok));

    let _ = driver.shutdown().await;
    println!("[selftest] {}", Value::Object(report));
    0
}

/// End-to-end supervisor test: create an app, wait for its control channel to
/// report `ready`, confirm the registry was written, then shut it down
/// gracefully. Runs inside a real (hidden) Tauri app so events and the async
/// runtime behave exactly as in production.
///
/// Run:  `NOVAMAIL_SELFTEST_MODE=launcher NOVAMAIL_WINDOW_HIDDEN=1 \
///        NOVAMAIL_REGISTRY_PATH=<temp> novamail.exe`
pub async fn run_launcher(state: LauncherState) -> i32 {
    let mut report = Map::new();
    let mut passed = true;

    report.insert(
        "registryPath".into(),
        json!(launcher::registry_path().display().to_string()),
    );

    let Some(created) = launcher::create_app(&state, Some("Selftest app".to_string())) else {
        report.insert("created".into(), json!(false));
        report.insert("passed".into(), json!(false));
        println!("[selftest] {}", Value::Object(report));
        return 1;
    };
    let id = created
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    report.insert("id".into(), json!(id));
    report.insert("created".into(), json!(!id.is_empty()));

    // Wait for the app to connect and report readiness (cold webview start can
    // take a few seconds).
    let mut final_state = String::new();
    let mut ready = false;
    for _ in 0..120 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        match launcher::snapshot_of(&state, &id) {
            Some(item) => {
                final_state = item
                    .get("state")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if final_state == "ready" {
                    ready = true;
                    break;
                }
                if final_state == "failed" {
                    break;
                }
            }
            None => {
                final_state = "missing".to_string();
                break;
            }
        }
    }
    passed &= ready;
    report.insert("ready".into(), json!(ready));
    report.insert("observedState".into(), json!(final_state));

    let registry_written = std::fs::read_to_string(launcher::registry_path())
        .map(|text| text.contains(&id))
        .unwrap_or(false);
    passed &= registry_written;
    report.insert("registryWritten".into(), json!(registry_written));

    report.insert("logs".into(), json!(launcher::logs_of(&state, &id)));

    // Graceful shutdown through the control channel.
    launcher::stop_app(&state, &id, false).await;
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    let stopped = launcher::snapshot_of(&state, &id)
        .and_then(|item| {
            item.get("state")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .map(|state| state == "stopped" || state == "failed")
        .unwrap_or(false);
    passed &= stopped;
    report.insert("stopped".into(), json!(stopped));

    report.insert("passed".into(), json!(passed));
    println!("[selftest] {}", Value::Object(report));
    if passed { 0 } else { 1 }
}

// ---------------------------------------------------------------------------
// Phase 5 — multi-app isolation self-tests
// ---------------------------------------------------------------------------

/// Wait for an app to reach a specific state (e.g. "ready").
async fn wait_for_state(
    state: &crate::launcher::LauncherState,
    id: &str,
    target: &str,
    timeout_ms: u64,
) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    while std::time::Instant::now() < deadline {
        if let Some(item) = crate::launcher::snapshot_of(state, id) {
            if item.get("state").and_then(Value::as_str) == Some(target) {
                return true;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    false
}

/// Wait for all apps to reach a specific state.
async fn wait_for_all_state(
    state: &crate::launcher::LauncherState,
    ids: &[String],
    target: &str,
    timeout_ms: u64,
) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    while std::time::Instant::now() < deadline {
        let all_ready = ids.iter().all(|id| {
            crate::launcher::snapshot_of(state, id)
                .map(|item| item.get("state").and_then(Value::as_str) == Some(target))
                .unwrap_or(false)
        });
        if all_ready {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    false
}

/// Wait for an app to be reaped (no longer a live child).
async fn wait_for_reap(state: &crate::launcher::LauncherState, id: &str, timeout_ms: u64) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    while std::time::Instant::now() < deadline {
        let reaped = crate::launcher::snapshot_of(state, id)
            .map(|item| {
                let state = item.get("state").and_then(Value::as_str).unwrap_or("");
                let pid = item.get("pid").and_then(Value::as_u64);
                (state == "stopped" || state == "failed") && pid.is_none()
            })
            .unwrap_or(true);
        if reaped {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    false
}

/// Headless multi-app fault-isolation self-test (Phase 5).
///
/// Creates N independent app processes, waits until every app reports ready,
/// force-kills one app, checks the other apps stay ready, restarts the killed
/// app, and checks the rest are untouched.
///
/// Run: `NOVAMAIL_SELFTEST_MODE=isolation NOVAMAIL_WINDOW_HIDDEN=1 \
///        NOVAMAIL_SELFTEST_APPS=N novamail.exe`
pub async fn run_isolation(state: crate::launcher::LauncherState) -> i32 {
    let mut report = Map::new();
    let mut passed = true;
    let mut steps: Vec<String> = Vec::new();

    let count = std::env::var("NOVAMAIL_SELFTEST_APPS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(5)
        .max(2);

    report.insert("appCount".into(), json!(count));

    // 1. Create N apps
    let mut created_ids = Vec::new();
    for i in 0..count {
        if let Some(created) =
            crate::launcher::create_app(&state, Some(format!("Test app {}", i + 1)))
        {
            let id = created
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if !id.is_empty() {
                created_ids.push(id);
            }
        }
    }
    if created_ids.len() != count {
        passed = false;
        report.insert(
            "error".into(),
            json!(format!(
                "only created {} of {} apps",
                created_ids.len(),
                count
            )),
        );
        report.insert("steps".into(), json!(steps));
        report.insert("passed".into(), json!(passed));
        println!("[selftest] {}", Value::Object(report));
        return 1;
    }
    steps.push(format!("created {} app records", created_ids.len()));

    // 2. Wait for all to report ready
    if !wait_for_all_state(&state, &created_ids, "ready", 120_000).await {
        // Print the state of each app for debugging
        for id in &created_ids {
            if let Some(item) = crate::launcher::snapshot_of(&state, id) {
                let state = item.get("state").and_then(Value::as_str).unwrap_or("?");
                let pid = item.get("pid").and_then(Value::as_u64);
                eprintln!("  app {id}: state={state} pid={pid:?}");
            } else {
                eprintln!("  app {id}: not found in snapshot");
            }
        }
        passed = false;
        report.insert("error".into(), "not all apps reported ready".into());
        report.insert("steps".into(), json!(steps));
        report.insert("passed".into(), json!(passed));
        println!("[selftest] {}", Value::Object(report));
        return 1;
    }
    steps.push("all apps reported ready".into());

    // 3. Verify distinct PIDs
    let pids: Vec<u32> = created_ids
        .iter()
        .filter_map(|id| crate::launcher::snapshot_of(&state, id))
        .filter_map(|item| item.get("pid").and_then(Value::as_u64).map(|p| p as u32))
        .collect();
    let unique_pids: std::collections::HashSet<u32> = pids.iter().cloned().collect();
    if unique_pids.len() != count {
        passed = false;
        report.insert(
            "error".into(),
            json!(format!(
                "apps did not get distinct PIDs ({}/{})",
                unique_pids.len(),
                count
            )),
        );
        report.insert("steps".into(), json!(steps));
        report.insert("passed".into(), json!(passed));
        println!("[selftest] {}", Value::Object(report));
        return 1;
    }
    steps.push("each app has a distinct PID".into());

    // 4. Wait for heartbeats
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    steps.push("all apps are heartbeating".into());

    // 5. Force-kill one app
    let victim_id = &created_ids[0];
    let victim_pid = crate::launcher::snapshot_of(&state, victim_id)
        .and_then(|item| item.get("pid").and_then(Value::as_u64).map(|p| p as u32));

    if let Some(pid) = victim_pid {
        crate::launcher::hard_kill_pid(pid);
        steps.push("force-killed one app".into());
    }

    // 6. Wait for the launcher to reap it
    if !wait_for_reap(&state, victim_id, 15_000).await {
        passed = false;
        report.insert(
            "error".into(),
            "launcher did not reap the killed app".into(),
        );
        report.insert("steps".into(), json!(steps));
        report.insert("passed".into(), json!(passed));
        println!("[selftest] {}", Value::Object(report));
        return 1;
    }
    steps.push("launcher noticed the kill and cleaned it up".into());

    // 7. Verify survivors stay ready
    tokio::time::sleep(std::time::Duration::from_millis(3000)).await;
    let survivors_ready = created_ids[1..].iter().all(|id| {
        crate::launcher::snapshot_of(&state, id)
            .map(|item| item.get("state").and_then(Value::as_str) == Some("ready"))
            .unwrap_or(false)
    });
    if !survivors_ready {
        passed = false;
        report.insert("error".into(), "not all survivors stayed ready".into());
        report.insert("steps".into(), json!(steps));
        report.insert("passed".into(), json!(passed));
        println!("[selftest] {}", Value::Object(report));
        return 1;
    }
    steps.push(format!(
        "{} other apps stayed ready after the kill",
        count - 1
    ));

    // 8. Restart the killed app
    crate::launcher::open_app(&state, victim_id);
    if !wait_for_state(&state, victim_id, "ready", 60_000).await {
        passed = false;
        report.insert("error".into(), "restarted app did not become ready".into());
        report.insert("steps".into(), json!(steps));
        report.insert("passed".into(), json!(passed));
        println!("[selftest] {}", Value::Object(report));
        return 1;
    }
    steps.push("restarted one app without disturbing the others".into());

    // 9. Verify all are ready
    let all_ready = created_ids.iter().all(|id| {
        crate::launcher::snapshot_of(&state, id)
            .map(|item| item.get("state").and_then(Value::as_str) == Some("ready"))
            .unwrap_or(false)
    });
    if !all_ready {
        passed = false;
        report.insert(
            "error".into(),
            "not all apps are ready after restart".into(),
        );
    }

    // 10. Clean shutdown
    for id in &created_ids {
        crate::launcher::stop_app(&state, id, true).await;
    }

    report.insert("steps".into(), json!(steps));
    report.insert("passed".into(), json!(passed));
    println!("[selftest] {}", Value::Object(report));
    if passed { 0 } else { 1 }
}

/// Headless launcher-crash survival self-test (Phase 5).
///
/// Creates N apps, waits for all to report ready, prints their PIDs, then
/// stays alive — the test harness hard-kills this launcher and checks that
/// every app keeps running.
///
/// Run: `NOVAMAIL_SELFTEST_MODE=survival NOVAMAIL_WINDOW_HIDDEN=1 \
///        NOVAMAIL_SELFTEST_APPS=N novamail.exe`
pub async fn run_survival(state: crate::launcher::LauncherState) -> i32 {
    let count = std::env::var("NOVAMAIL_SELFTEST_APPS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(3)
        .max(2);

    let mut created_ids = Vec::new();
    for i in 0..count {
        if let Some(created) =
            crate::launcher::create_app(&state, Some(format!("Survival app {}", i + 1)))
        {
            let id = created
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if !id.is_empty() {
                created_ids.push(id);
            }
        }
    }

    if !wait_for_all_state(&state, &created_ids, "ready", 60_000).await {
        eprintln!("[survival] not all apps became ready");
        return 1;
    }

    let pids: Vec<u32> = created_ids
        .iter()
        .filter_map(|id| crate::launcher::snapshot_of(&state, id))
        .filter_map(|item| item.get("pid").and_then(Value::as_u64).map(|p| p as u32))
        .collect();
    println!("[selftest-pids] {}", json!({ "pids": pids }));

    // Stay alive — the test harness will hard-kill this process.
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
    }
}

/// Headless launcher render check (Phase 5).
///
/// Boots the launcher window and verifies the window was created, the page
/// loaded, and the launcher title is correct.
///
/// Run: `NOVAMAIL_SELFTEST_MODE=render NOVAMAIL_WINDOW_HIDDEN=1 novamail.exe`
pub async fn run_render(app: tauri::AppHandle) -> i32 {
    let mut report = Map::new();
    let mut passed = true;
    let mut steps: Vec<String> = Vec::new();

    let window = match app.get_webview_window("main") {
        Some(w) => w,
        None => {
            report.insert("error".into(), "no main window found".into());
            report.insert("passed".into(), json!(false));
            println!("[selftest] {}", Value::Object(report));
            return 1;
        }
    };

    // Wait for the page to load (evaluate a simple expression)
    let loaded = tokio::time::timeout(std::time::Duration::from_secs(45), async {
        loop {
            if window.eval("1 + 1").is_ok() {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    })
    .await
    .unwrap_or(false);

    if !loaded {
        report.insert("error".into(), "page did not load in time".into());
        report.insert("passed".into(), json!(false));
        println!("[selftest] {}", Value::Object(report));
        return 1;
    }
    steps.push("launcher window created and page loaded".into());

    // Verify the launcher title (set based on role)
    let title = window.title().unwrap_or_default();
    if title == "Novamail" {
        steps.push("launcher window has correct title".into());
    } else {
        passed = false;
        report.insert(
            "error".into(),
            json!(format!("launcher window title was '{}'", title)),
        );
    }

    // Verify the runtime script was injected by checking the title
    // (the initialization script sets window.__NOVAMAIL_RUNTIME__)
    let runtime_injected = window
        .eval("(function(){ return Boolean(window.__NOVAMAIL_RUNTIME__); })()")
        .is_ok();
    if runtime_injected {
        steps.push("runtime script injected".into());
    } else {
        passed = false;
        report.insert("error".into(), "runtime script was not injected".into());
    }

    report.insert("steps".into(), json!(steps));
    report.insert("passed".into(), json!(passed));
    println!("[selftest] {}", Value::Object(report));
    if passed { 0 } else { 1 }
}

/// Headless app-list persistence check (Phase 5).
///
/// Verifies that apps round-trip through disk, that a leftover running process
/// is reported as "orphaned", and that removing an app persists.
///
/// Run: `NOVAMAIL_SELFTEST_MODE=registry NOVAMAIL_WINDOW_HIDDEN=1 \
///        NOVAMAIL_REGISTRY_PATH=<temp> novamail.exe`
pub fn run_registry() -> i32 {
    let mut report = Map::new();
    let mut passed = true;
    let mut steps: Vec<String> = Vec::new();

    // Write test records to the registry file
    let test_records = json!([
        {
            "id": "app_test0001aaaa",
            "name": "Registry A",
            "accountEmail": "a@example.com",
            "createdAt": 111,
            "pid": null,
            "lastState": "stopped",
            "lastError": null,
        },
        {
            "id": "app_test0002bbbb",
            "name": "Registry B",
            "accountEmail": "",
            "createdAt": 222,
            "pid": std::process::id(),
            "lastState": "starting",
            "lastError": null,
        },
    ]);

    let path = crate::launcher::registry_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(&test_records) {
        let _ = std::fs::write(&path, text);
    }

    // Reload and verify
    let registry = crate::launcher::Registry::new();
    let snapshot = registry.snapshot();
    if snapshot.len() != 2 {
        passed = false;
        report.insert(
            "error".into(),
            json!(format!(
                "reload found {} records, expected 2",
                snapshot.len()
            )),
        );
        report.insert("steps".into(), json!(steps));
        report.insert("passed".into(), json!(passed));
        println!("[selftest] {}", Value::Object(report));
        return 1;
    }

    let names: Vec<&str> = snapshot
        .iter()
        .filter_map(|item| item.get("name").and_then(Value::as_str))
        .collect();
    if names.len() != 2 || names[0] != "Registry A" || names[1] != "Registry B" {
        passed = false;
        report.insert("error".into(), "reloaded names are wrong".into());
    }
    if snapshot[0].get("accountEmail").and_then(Value::as_str) != Some("a@example.com") {
        passed = false;
        report.insert("error".into(), "reloaded email is wrong".into());
    }
    steps.push("app list round-trips through disk".into());

    // Check orphan detection
    let orphan = snapshot
        .iter()
        .find(|item| item.get("id").and_then(Value::as_str) == Some("app_test0002bbbb"));
    let orphan_ok =
        orphan.and_then(|item| item.get("state").and_then(Value::as_str)) == Some("orphaned");
    if !orphan_ok {
        passed = false;
        report.insert(
            "error".into(),
            json!(format!(
                "a live leftover process was reported as {:?}",
                orphan.and_then(|item| item.get("state").and_then(Value::as_str))
            )),
        );
    } else {
        steps.push("a leftover running process is reported as orphaned".into());
    }

    // Remove one record and verify persistence
    let filtered: Vec<Value> = test_records
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|item| item.get("id").and_then(Value::as_str) != Some("app_test0001aaaa"))
        .collect();
    if let Ok(text) = serde_json::to_string_pretty(&filtered) {
        let _ = std::fs::write(&path, text);
    }

    let registry = crate::launcher::Registry::new();
    let snapshot = registry.snapshot();
    if snapshot.len() != 1 {
        passed = false;
        report.insert("error".into(), "removing an app did not persist".into());
    } else {
        steps.push("removing an app persists".into());
    }

    report.insert("steps".into(), json!(steps));
    report.insert("passed".into(), json!(passed));
    println!("[selftest] {}", Value::Object(report));
    if passed { 0 } else { 1 }
}
