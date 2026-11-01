//! `chromiumoxide` implementation of [`BrowserDriver`].
//!
//! Drives the vendored Clearcote Chromium over CDP. The browser process is
//! spawned by chromiumoxide (`Browser::launch`), its stderr is polled for the
//! DevTools websocket URL, and the resulting event handler is drained on a
//! dedicated task for as long as the browser lives.
//!
//! HTML is rendered by writing it to a temporary file and navigating a
//! `file://` URL: desktop runtime served the markup through an intercepted request for
//! exactly the same reason (`page.setContent()` never settles on the Clearcote
//! engine), and a local file works for documents of any size. Once loaded we
//! wait for images and web fonts so the text is laid out with its final
//! typeface before `Page.printToPDF` runs.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::dom::SetFileInputFilesParams;
use chromiumoxide::cdp::browser_protocol::emulation::{
    ClearDeviceMetricsOverrideParams, SetDeviceMetricsOverrideParams,
};
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams, DispatchMouseEventType,
    InsertTextParams, MouseButton,
};
use chromiumoxide::cdp::browser_protocol::page::{
    CaptureScreenshotFormat, EventJavascriptDialogOpening, HandleJavaScriptDialogParams,
    PrintToPdfParams, Viewport,
};
use chromiumoxide::cdp::js_protocol::runtime::EvaluateParams;
use chromiumoxide::page::{Page, ScreenshotParams};
use futures::StreamExt;

use super::humanize::{HUMAN_TYPE_MAX_CHARS, Humanizer, MoveStep, TypeOp};
use super::{
    BoxFuture, BrowserDriver, DialogWatcher, DriverConfig, Key, PageId, PdfOptions,
    paper_size_inches,
};

/// Ceilings so a wedged browser can never hang the app forever. desktop runtime
/// disables Playwright's timeouts entirely; these only exist as a backstop.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(15);
const LOAD_TIMEOUT: Duration = Duration::from_secs(10);
const NAVIGATION_TIMEOUT: Duration = Duration::from_secs(45);
const PRINT_TIMEOUT: Duration = Duration::from_secs(30);
/// Per-CDP-command timeout. chromiumoxide's own default is 30s, which Gmail can
/// exceed while its main thread is busy loading the inbox/contacts; a short
/// timeout then surfaces as an opaque "Request timed out." mid-compose.
const CDP_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// How many rendered pages to keep warm for reuse. Each idle page holds a
/// renderer, so the pool is deliberately small; it exists to avoid CDP
/// target churn between conversions, not to keep a large fleet.
const IDLE_PAGE_LIMIT: usize = 4;

/// Resolve once every `<img>` has loaded or failed.
const WAIT_FOR_IMAGES_JS: &str = r#"(() => Promise.all(
  Array.from(document.images).map((image) => (image.complete
    ? Promise.resolve()
    : new Promise((resolve) => {
        image.addEventListener('load', resolve, { once: true });
        image.addEventListener('error', resolve, { once: true });
      })))
).then(() => true))()"#;

/// Resolve once web fonts are ready so text uses its final typeface.
const WAIT_FOR_FONTS_JS: &str = r#"(() => (document.fonts && document.fonts.ready
  ? document.fonts.ready.then(() => true)
  : true))()"#;

/// CDP modifier bit for Control (Alt=1, Ctrl=2, Meta=4, Shift=8).
const MOD_CTRL: i64 = 2;
/// CDP modifier bit for Meta/Command.
const MOD_META: i64 = 4;
/// CDP modifier bit for Shift (see the same table).
const MOD_SHIFT: i64 = 8;
/// Engine flags the driver always supplies, independent of the caller.
///
/// This is chromiumoxide's own `DEFAULT_ARGS` with the switches that expose the
/// session as automated removed:
///
/// * `--enable-automation` raises Chrome's *"being controlled by automated test
///   software"* infobar. `--disable-infobars` no longer suppresses it in modern
///   Chrome, so the only reliable fix is to not pass the flag at all.
/// * `--enable-blink-features=IdleDetection` turns on an API that is off by
///   default in a real browser.
///
/// [`ChromiumDriver::launch`] calls `.disable_default_args()` so chromiumoxide
/// stops injecting these, and passes this list instead; the caller's own
/// switches (the persona's privacy `--disable-features`, the geo `--lang`)
/// still follow and therefore win.
const BASELINE_ARGS: &[&str] = &[
    "--disable-background-networking",
    "--disable-blink-features=AutomationControlled",
    "--enable-features=NetworkService,NetworkServiceInProcess",
    "--disable-background-timer-throttling",
    "--disable-backgrounding-occluded-windows",
    "--disable-breakpad",
    "--disable-client-side-phishing-detection",
    "--disable-component-extensions-with-background-pages",
    "--disable-default-apps",
    "--disable-dev-shm-usage",
    "--disable-features=TranslateUI,CalculateNativeWinOcclusion,IntensiveWakeUpThrottling,TimerThrottlingForHiddenFrames,ThrottleDisplayNoneAndVisibilityHiddenCrossOriginIframes",
    "--disable-hang-monitor",
    "--disable-ipc-flooding-protection",
    "--disable-popup-blocking",
    "--disable-prompt-on-repost",
    "--disable-renderer-backgrounding",
    "--disable-sync",
    "--force-color-profile=srgb",
    "--metrics-recording-only",
    "--no-first-run",
    "--password-store=basic",
    "--use-mock-keychain",
    "--lang=en-US",
];

/// Prepend [`BASELINE_ARGS`] to `extra`, collapsing switches that share a *name*
/// so the final command line carries each switch exactly once.
///
/// chromiumoxide stores every argument in a `HashMap` keyed by the whole string
/// and re-emits it as `--<key>` in arbitrary order. Because the key is the whole
/// string, two arguments that differ only in their value — the baseline
/// `lang=en_US` versus the persona's `lang=en-US`, or the baseline
/// `disable-features=TranslateUI` versus the persona's Privacy Sandbox list —
/// would BOTH survive and be emitted in a random order, leaving Chromium to pick
/// a different value on every launch. That makes the session's locale and
/// feature set non-deterministic (a shape no real browser produces).
///
/// Collapsing by switch name fixes that: the caller's value (added after the
/// baseline) wins for scalars, while `enable-features` / `disable-features` are
/// unioned so the Privacy Sandbox list is never dropped.
///
/// The entries are supplied as bare keys (`no-first-run`,
/// `force-color-profile=srgb`) because chromiumoxide's `Arg` treats the whole
/// string as the switch *name* and prepends its own `--`. Passing them with the
/// leading dashes we write in [`BASELINE_ARGS`] would emit `----no-first-run`,
/// which Chromium silently ignores — that is what previously let the ungoogled
/// first-run page (and every other flag, including `--incognito`) slip through.
fn merge_baseline_args(extra: &[String]) -> Vec<String> {
    let mut order: Vec<String> = Vec::with_capacity(BASELINE_ARGS.len() + extra.len());
    let mut by_name: HashMap<String, String> = HashMap::new();
    for arg in BASELINE_ARGS.iter().copied().chain(extra.iter().map(String::as_str)) {
        let arg = arg.trim_start_matches('-');
        if arg.is_empty() {
            continue;
        }
        let (name, value) = match arg.split_once('=') {
            Some((name, value)) => (name, value.to_string()),
            None => (arg, String::new()),
        };
        match by_name.get_mut(name) {
            Some(existing) => {
                if name == "disable-features" || name == "enable-features" {
                    for part in value.split(',').map(str::trim).filter(|part| !part.is_empty()) {
                        if !existing.split(',').any(|seen| seen == part) {
                            if !existing.is_empty() {
                                existing.push(',');
                            }
                            existing.push_str(part);
                        }
                    }
                } else if !value.is_empty() {
                    // The caller's switch (persona/geo) arrives after the
                    // baseline, so its value wins.
                    *existing = value;
                }
            }
            None => {
                order.push(name.to_string());
                by_name.insert(name.to_string(), value);
            }
        }
    }
    order
        .into_iter()
        .map(|name| match by_name.remove(&name) {
            Some(value) if !value.is_empty() => format!("{name}={value}"),
            _ => name,
        })
        .collect()
}

/// The CDP description of a [`Key`]: `(key, code, virtual-key-code, text,
/// modifiers)`. `text` is only set for keys that should produce a character
/// (Enter's carriage return); shortcut letters must not, or Chromium would
/// insert them into the field.
fn key_spec(key: Key) -> (&'static str, &'static str, i64, Option<&'static str>, i64) {
    let primary = if cfg!(target_os = "macos") {
        MOD_META
    } else {
        MOD_CTRL
    };
    match key {
        Key::Enter => ("Enter", "Enter", 13, Some("\r"), 0),
        Key::Backspace => ("Backspace", "Backspace", 8, None, 0),
        Key::Escape => ("Escape", "Escape", 27, None, 0),
        Key::SelectAll => ("a", "KeyA", 65, None, primary),
        Key::Paste => ("v", "KeyV", 86, None, primary),
        Key::Submit => ("Enter", "Enter", 13, None, primary),
    }
}

pub struct ChromiumDriver {
    /// `Browser` needs `&mut` to close and reap, so it lives behind an async
    /// lock. Page *creation* borrows it through the lock; page *operations* use
    /// cloned `Page` handles and never touch this lock.
    browser: tokio::sync::Mutex<Browser>,
    pages: Mutex<HashMap<PageId, Page>>,
    /// Render pages returned with [`BrowserDriver::release_page`], kept warm so
    /// the next conversion can skip `new_page`/`close_page` on the CDP target.
    idle_pages: Mutex<Vec<PageId>>,
    next_page: AtomicU64,
    closed: Arc<AtomicBool>,
    handler: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Removed on shutdown. `None` would mean a caller-supplied persistent
    /// profile that the driver must not delete.
    profile_dir: Option<PathBuf>,
    /// The human hand driving pointer and keyboard, when enabled for this launch.
    humanizer: Option<Mutex<Humanizer>>,
}

impl ChromiumDriver {
    /// Launch Chromium and start draining its CDP event stream.
    pub async fn launch(config: DriverConfig) -> Result<Self, String> {
        let mut builder = BrowserConfig::builder()
            .chrome_executable(&config.executable)
            .launch_timeout(LAUNCH_TIMEOUT)
            // chromiumoxide's built-in defaults include `--enable-automation`
            // (the "controlled by automated test software" infobar) and
            // `--enable-blink-features=IdleDetection`. Drop them and supply our
            // own tell-free baseline instead.
            .disable_default_args()
            .args(merge_baseline_args(&config.extra_args))
            // No viewport emulation: the page uses the real window size, which
            // is what desktop runtime's `viewport: null` did.
            .viewport(None)
            // Gmail's renderer can stall for longer than chromiumoxide's 30s
            // default while the inbox syncs; give commands more headroom so a
            // slow-but-valid response is not dropped as a timeout.
            .request_timeout(CDP_REQUEST_TIMEOUT);
        if !config.headless {
            builder = builder.with_head();
        }
        if let Some(dir) = config.user_data_dir.clone() {
            // Pre-seed the first-run sentinel. The bundled ungoogled Chromium
            // opens its own `chrome://ungoogled-first-run` tab at startup when
            // this file is absent — and it keys off the file rather than the
            // `--no-first-run` switch we already pass. Every production launch
            // uses a throwaway profile, so without this the operator sees a
            // stray first-run tab beside the Gmail window.
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(dir.join("First Run"), "");
            builder = builder.user_data_dir(dir);
        }
        if let Some((width, height)) = config.window_size {
            builder = builder.window_size(width, height);
        }
        let browser_config = builder.build().map_err(|error| error.to_string())?;

        let executable = config.executable.display().to_string();
        let (mut browser, mut handler) = Browser::launch(browser_config)
            .await
            .map_err(|error| format!("Could not launch Chromium at {executable}: {error}"))?;

        // Leave the browser at normal priority. Boosting to ABOVE_NORMAL
        // causes CPU contention when multiple apps each run their own
        // Chromium — they fight for the CPU and the whole system hangs.
        // The OS scheduler handles normal-priority browsers fairly.
        #[cfg(windows)]
        if !config.headless {
            let _ = browser
                .get_mut_child()
                .and_then(|child| child.as_mut_inner().id());
        }

        // The handler drives the websocket connection; it has to be polled or
        // the connection stalls and every command times out.
        let closed = Arc::new(AtomicBool::new(false));
        let handler_closed = closed.clone();
        let join = tokio::spawn(async move {
            while let Some(event) = handler.next().await {
                if let Err(error) = event {
                    log::debug!("[browser] CDP handler event error: {error}");
                }
            }
            handler_closed.store(true, Ordering::Release);
        });

        let humanizer = if config.humanize {
            Some(Mutex::new(Humanizer::new(config.humanize_seed.as_deref())))
        } else {
            None
        };

        Ok(Self {
            browser: tokio::sync::Mutex::new(browser),
            pages: Mutex::new(HashMap::new()),
            idle_pages: Mutex::new(Vec::new()),
            next_page: AtomicU64::new(1),
            closed,
            handler: Mutex::new(Some(join)),
            // `None` keeps a caller-supplied profile that must outlive the
            // driver (a persistent live-diagnostic session).
            profile_dir: if config.keep_profile {
                None
            } else {
                config.user_data_dir
            },
            humanizer,
        })
    }

    /// Lock the human hand, returning a stable error instead of panicking on a poisoned mutex.
    fn humanizer(&self) -> Option<std::sync::MutexGuard<'_, Humanizer>> {
        self.humanizer.as_ref().and_then(|cell| cell.lock().ok())
    }

    /// Dispatch the pointer warm-up before the first keyboard interaction of a session.
    ///
    /// A page whose collected pointer entropy is zero sees its first `keydown` as the
    /// session's very first input event — a shape no real user produces. The warm-up is
    /// an ambient pointer burst (mouse *moves* only, never a button), local-sleeped like
    /// every other step, and strictly best-effort: a dispatch failure must never fail
    /// the send that follows.
    async fn ensure_warmup(&self, page: &Page) {
        let needs = self
            .humanizer()
            .map(|hand| hand.needs_warmup())
            .unwrap_or(false);
        if !needs {
            return;
        }
        // The viewport must be read before the human hand is locked: the guard is a
        // `std::sync` mutex and must never be held across an await.
        let (width, height) = viewport_size(page).await;
        let steps = {
            // Scope the guard so it is provably dropped before the await below.
            let Some(mut hand) = self.humanizer() else {
                return;
            };
            hand.take_warmup((width, height))
        };
        if steps.is_empty() {
            return;
        }
        log::debug!("[browser] dispatching a {}-step pointer warm-up", steps.len());
        let _ = dispatch_moves(page, &steps).await;
    }


    /// Look up a live page handle by id.
    fn page(&self, id: PageId) -> Result<Page, String> {
        let pages = self.pages.lock().map_err(|error| error.to_string())?;
        pages
            .get(&id)
            .cloned()
            .ok_or_else(|| format!("Unknown browser page {id}."))
    }

    /// Write the markup to a fresh temp file and return its path.
    fn write_html_to_temp(&self, html: &str) -> Result<PathBuf, String> {
        let dir = std::env::temp_dir().join("novamail-render");
        std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
        let path = dir.join(format!(
            "render-{}-{}.html",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&path, html.as_bytes()).map_err(|error| error.to_string())?;
        Ok(path)
    }
}



/// Evaluate `script` with promise awaiting enabled and ignore both the result
/// and any error. Used for the best-effort waits before printing.
async fn evaluate_and_wait(page: &Page, script: &str) {
    let Ok(params) = EvaluateParams::builder()
        .expression(script)
        .await_promise(true)
        .build()
    else {
        return;
    };
    let _ = tokio::time::timeout(LOAD_TIMEOUT, page.evaluate(params)).await;
}

/// Walk a planned pointer path with `Input.dispatchMouseEvent` and off-protocol sleeps.
///
/// The CDP round-trip is only issued once per sample; the *pauses* are local `tokio` sleeps, never a
/// protocol call. A `page.waitForTimeout`-style sleep is itself a round-trip per step, so it would
/// emit exactly the protocol chatter the humanisation exists to avoid.
async fn dispatch_moves(page: &Page, steps: &[MoveStep]) -> Result<(), String> {
    for step in steps {
        let params = DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MouseMoved)
            .x(step.x)
            .y(step.y)
            .build()
            .map_err(|error| error.to_string())?;
        page.execute(params).await.map_err(|error| error.to_string())?;
        if step.sleep_ms > 0.0 {
            tokio::time::sleep(Duration::from_millis(step.sleep_ms as u64)).await;
        }
    }
    Ok(())
}

/// Dispatch a planned typing sequence key-by-key, honouring the inter-op sleeps.
///
/// Every `KeyDown`/`KeyUp` is one CDP `Input.dispatchKeyEvent` round-trip; the dwell/flight
/// pauses are local `tokio` sleeps, never a protocol call — the same rule [`dispatch_moves`]
/// applies to pointer samples. This is the path the Gmail compose flow now takes for short
/// fields typed through [`BrowserDriver::insert_text`] (bodies up to
/// [`HUMAN_TYPE_MAX_CHARS`]); the subject and recipients take the atomic protocol insert,
/// so the keystroke dynamics a behavioural collector sees (hold time, flight time,
/// rollover, Shift pairs) match the plan in [`super::humanize::Humanizer::plan_typing`].
async fn dispatch_type_ops(page: &Page, ops: &[TypeOp]) -> Result<(), String> {
    for op in ops {
        match op {
            TypeOp::Sleep(ms) => {
                if *ms > 0 {
                    tokio::time::sleep(Duration::from_millis(*ms)).await;
                }
            }
            TypeOp::InsertText(run) => {
                let params = InsertTextParams::new(run.clone());
                page.execute(params).await.map_err(|error| error.to_string())?;
            }
            TypeOp::ShiftDown => dispatch_shift(page, true).await?,
            TypeOp::ShiftUp => dispatch_shift(page, false).await?,
            TypeOp::KeyDown {
                key,
                code,
                vk,
                text,
                shift,
            } => {
                let mut down = DispatchKeyEventParams::builder()
                    .key(key.clone())
                    .code(code.clone())
                    .windows_virtual_key_code(*vk)
                    .native_virtual_key_code(*vk);
                if *shift {
                    down = down.modifiers(MOD_SHIFT);
                }
                // Puppeteer's rule: a key that produces text uses `keyDown` (so the
                // renderer inserts the character); everything else is `rawKeyDown`.
                let is_raw = text.is_empty();
                if !is_raw {
                    down = down.text(text.clone());
                }
                let type_tag = if is_raw {
                    DispatchKeyEventType::RawKeyDown
                } else {
                    DispatchKeyEventType::KeyDown
                };
                let params = down
                    .r#type(type_tag)
                    .build()
                    .map_err(|error| error.to_string())?;
                page.execute(params).await.map_err(|error| error.to_string())?;
            }
            TypeOp::KeyUp {
                key,
                code,
                vk,
                shift,
            } => {
                let mut up = DispatchKeyEventParams::builder()
                    .r#type(DispatchKeyEventType::KeyUp)
                    .key(key.clone())
                    .code(code.clone())
                    .windows_virtual_key_code(*vk)
                    .native_virtual_key_code(*vk);
                if *shift {
                    up = up.modifiers(MOD_SHIFT);
                }
                let params = up.build().map_err(|error| error.to_string())?;
                page.execute(params).await.map_err(|error| error.to_string())?;
            }
        }
    }
    Ok(())
}

/// Press or release the physical Shift key as its own event, the way a hand pairs it
/// with the shifted letter.
async fn dispatch_shift(page: &Page, press: bool) -> Result<(), String> {
    let params = DispatchKeyEventParams::builder()
        .key("Shift")
        .code("ShiftLeft")
        .windows_virtual_key_code(16)
        .native_virtual_key_code(16);
    let type_tag = if press {
        DispatchKeyEventType::RawKeyDown
    } else {
        DispatchKeyEventType::KeyUp
    };
    let params = params
        .r#type(type_tag)
        .build()
        .map_err(|error| error.to_string())?;
    page.execute(params)
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// `true` when the focused element now holds exactly `text` (whitespace-normalised).
///
/// The verification only gates the humanised path: on any miss the caller replays the
/// proven atomic insertion, so a false negative costs a fallback, never the message.
async fn insertion_verified(page: &Page, text: &str) -> bool {
    let Ok(expected) = serde_json::to_string(text) else {
        return false;
    };
    let js = format!(
        r#"(() => {{
  const expected = {expected};
  const a = document.activeElement;
  if (!a) return false;
  const raw = 'value' in a ? String(a.value) : String(a.innerText || a.textContent || '');
  const norm = (value) => String(value || '').replace(/\u00a0/g, ' ').replace(/\s+/g, ' ').trim();
  return norm(raw) === norm(expected);
}})()"#
    );
    let Ok(params) = EvaluateParams::builder()
        .expression(&js)
        .await_promise(false)
        .build()
    else {
        return false;
    };
    page.evaluate(params)
        .await
        .ok()
        .and_then(|result| {
            result
                .value()
                .and_then(|value| value.as_bool())
                .map(|verified| verified)
        })
        .unwrap_or(false)
}

/// Empty the focused element without moving focus, so a fallback insertion cannot
/// append on top of characters a failed humanised attempt left behind.
async fn clear_focused_field(page: &Page) {
    const CLEAR_FOCUS_JS: &str = r#"(() => {
  const a = document.activeElement;
  if (!a) return false;
  if ('value' in a) {
    try { if (a.select) a.select(); } catch (e) {}
    try { a.value = ''; } catch (e) {}
    try { a.dispatchEvent(new Event('input', { bubbles: true })); } catch (e) {}
    return true;
  }
  if (a.isContentEditable) {
    try { a.focus(); } catch (e) {}
    try { document.execCommand('selectAll', false, null); } catch (e) {}
    try { document.execCommand('delete', false, null); } catch (e) {}
    try { a.dispatchEvent(new Event('input', { bubbles: true, cancelable: true })); } catch (e) {}
    return true;
  }
  return false;
})()"#;
    let Ok(params) = EvaluateParams::builder()
        .expression(CLEAR_FOCUS_JS)
        .build()
    else {
        return;
    };
    let _ = page.evaluate(params).await;
}

/// Press and release the left button at `target`, holding it `hold_ms` like a finger.
///
/// Reserved for the humanised pointer-click path (see [`Humanizer::plan_click_box`]). The current
/// Gmail flow presses compose/send through the DOM (`element.click()`) because the CDP
/// `Input.dispatchMouseEvent` path has crashed Gmail renderers, so this stays a ready building
/// block rather than a live call.
#[allow(dead_code)]
async fn dispatch_press(page: &Page, target: (f64, f64), hold_ms: f64) -> Result<(), String> {
    let down = DispatchMouseEventParams::builder()
        .r#type(DispatchMouseEventType::MousePressed)
        .x(target.0)
        .y(target.1)
        .button(MouseButton::Left)
        .buttons(1)
        .click_count(1)
        .build()
        .map_err(|error| error.to_string())?;
    page.execute(down).await.map_err(|error| error.to_string())?;

    tokio::time::sleep(Duration::from_millis(hold_ms.max(1.0) as u64)).await;

    let up = DispatchMouseEventParams::builder()
        .r#type(DispatchMouseEventType::MouseReleased)
        .x(target.0)
        .y(target.1)
        .button(MouseButton::Left)
        .buttons(0)
        .click_count(1)
        .build()
        .map_err(|error| error.to_string())?;
    page.execute(up).await.map_err(|error| error.to_string())?;
    Ok(())
}

/// Characters that Gmail treats as separators when they arrive as real key events:
/// peoplekit chips the To-field on a keyed comma, so a comma-separated recipient list
/// must take the atomic path (a key-driven comma would chip mid-insert and duplicate
/// the remainder into a second token). Any *other* text going through
/// [`BrowserDriver::insert_text`] — bodies — keeps the keystroke realism; the
/// subject takes the atomic [`BrowserDriver::insert_text_fragment`] path in the
/// compose flow.
fn looks_like_recipient_list(text: &str) -> bool {
    let parts: Vec<&str> = text.split(',').map(str::trim).collect();
    parts.len() > 1 && parts.iter().all(|part| part.contains('@'))
}

/// `true` when `text` *is* a single bare email address (no spaces, exactly one `@`,
/// dotted domain). The To field receives exactly one address per message, and typing
/// it key-by-key costs seconds of dwell/flight pauses per send. Such text is
/// injected in one protocol operation instead — the very path the compose flow used
/// for every field before humanised typing existed, and the one its chip commit is
/// proven against. Subjects and bodies that merely quote an address among other
/// words are not address-shaped and keep the keystroke realism.
fn looks_like_recipient_address(text: &str) -> bool {
    let text = text.trim();
    let mut parts = text.split('@');
    // Exactly one `@`; anything else (none, two, …) is not address-shaped.
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && domain.contains('.')
        && text.chars().all(|character| character.is_ascii_graphic())
}

/// The page's real viewport in CSS pixels, or a conservative default when it cannot be read.
async fn viewport_size(page: &Page) -> (f64, f64) {
    const DEFAULT: (f64, f64) = (1280.0, 800.0);
    let Ok(params) = EvaluateParams::builder()
        .expression("({ w: innerWidth || 1280, h: innerHeight || 800 })")
        .build()
    else {
        return DEFAULT;
    };
    let Ok(result) = page.evaluate(params).await else {
        return DEFAULT;
    };
    let Some(value) = result.value() else {
        return DEFAULT;
    };
    let width = value.get("w").and_then(serde_json::Value::as_f64).unwrap_or(1280.0);
    let height = value.get("h").and_then(serde_json::Value::as_f64).unwrap_or(800.0);
    if width > 0.0 && height > 0.0 {
        (width, height)
    } else {
        DEFAULT
    }
}

impl BrowserDriver for ChromiumDriver {
    fn engine(&self) -> &'static str {
        "chromiumoxide"
    }

    fn is_browser_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    fn new_page<'a>(&'a self, url: Option<&'a str>) -> BoxFuture<'a, Result<PageId, String>> {
        Box::pin(async move {
            let target = url.unwrap_or("about:blank").to_string();
            let page = {
                let browser = self.browser.lock().await;
                browser
                    .new_page(target)
                    .await
                    .map_err(|error| error.to_string())?
            };
            let id = self.next_page.fetch_add(1, Ordering::Relaxed);
            self.pages
                .lock()
                .map_err(|error| error.to_string())?
                .insert(id, page);
            Ok(id)
        })
    }

    fn goto<'a>(&'a self, page: PageId, url: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let handle = self.page(page)?;
            let target = url.to_string();
            tokio::time::timeout(NAVIGATION_TIMEOUT, handle.goto(target))
                .await
                .map_err(|_| format!("Timed out loading {url}."))?
                .map_err(|error| error.to_string())?;
            Ok(())
        })
    }

    fn insert_text<'a>(&'a self, page: PageId, text: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let handle = self.page(page)?;
            // Humanised path: typed text is driven key-by-key through the plan in
            // [`Humanizer::plan_typing`] when the hand is enabled for this launch and the
            // text is short (see [`HUMAN_TYPE_MAX_CHARS`]). Everything a keystroke-dynamics
            // collector measures — hold time, flight time, rollover, thinking pauses,
            // Shift handling — then matches the persona instead of one atomic protocol
            // insertion with no keyboard events at all.
            //
            // The plan is verified after dispatch: on any CDP error or a value that did
            // not land (Gmail's recipient/subject acceptance is the caller's own check)
            // the field is cleared and the proven atomic path below replays it.
            let mut typed_attempted = false;
            if self.humanizer.is_some() {
                let char_count = text.chars().count();
                if char_count > 0
                    && char_count <= HUMAN_TYPE_MAX_CHARS
                    && !looks_like_recipient_list(text)
                    // A single recipient address is injected in one operation
                    // instead of key-by-key: key-drive costs seconds of pauses
                    // per message, and the atomic path is the shape Gmail's
                    // chip commit has always accepted.
                    && !looks_like_recipient_address(text)
                {
                    self.ensure_warmup(&handle).await;
                    let plan = match self.humanizer() {
                        Some(mut hand) => hand.plan_typing(text),
                        None => Vec::new(),
                    };
                    if !plan.is_empty() {
                        typed_attempted = true;
                        match dispatch_type_ops(&handle, &plan).await {
                            Err(error) => log::debug!(
                                "[browser] humanised typing failed ({error}); falling back to the atomic insertion"
                            ),
                            Ok(()) => {
                                if insertion_verified(&handle, text).await {
                                    return Ok(());
                                }
                                log::debug!(
                                    "[browser] humanised typing did not verify; falling back to the atomic insertion"
                                );
                            }
                        }
                    }
                }
            }
            if typed_attempted {
                // Characters from the failed attempt are still in the field: clear it so
                // the legacy join below cannot double-insert.
                clear_focused_field(&handle).await;
            }
            // Fallback: one atomic `Input.insertText`. This is the exact path the older
            // build used for every field, so the failure mode is known-proven. On this
            // custom Chromium build the CDP `Input.insertText` can hang when the active
            // element is Gmail's complex recipient combobox; fall back to DOM-based
            // insertion when the CDP command fails or times out (treated as error).
            let params = InsertTextParams::new(text.to_string());
            if let Err(error) = handle.execute(params).await {
                // DOM fallback: set value/text and dispatch input events
                let text_json = serde_json::to_string(text).map_err(|e| e.to_string())?;
                let js = format!(
                    r#"(() => {{
  const t = {};
  const a = document.activeElement;
  if (!a) return false;
  // Try input/textarea
  if ('value' in a) {{
    try {{ a.value = t; }} catch (e) {{}}
    try {{ a.dispatchEvent(new Event('input', {{bubbles:true}})); }} catch (e) {{}}
    try {{ a.dispatchEvent(new Event('change', {{bubbles:true}})); }} catch (e) {{}}
    return true;
  }}
  // Contenteditable
  try {{
    a.focus();
    // Prefer direct set for comboboxes (avoids selectAll side effects)
    try {{ a.textContent = t; }} catch (e) {{}}
    try {{ if (!a.textContent) a.innerText = t; }} catch (e) {{}}
    try {{ a.dispatchEvent(new Event('input', {{bubbles:true,cancelable:true}})); }} catch (e) {{}}
    try {{ a.dispatchEvent(new InputEvent('input', {{bubbles:true,cancelable:true,inputType:'insertText',data:t}})); }} catch (e) {{}}
    try {{ a.dispatchEvent(new Event('change', {{bubbles:true}})); }} catch (e) {{}}
    const got = (a.textContent || '').replace(/\u00a0/g,' ').trim();
    if (got === t) return true;
    // Fallback to execCommand if direct set didn't take effect
    try {{ document.execCommand('selectAll', false, null); document.execCommand('delete', false, null); document.execCommand('insertText', false, t); }} catch (e) {{}}
    return ((a.textContent || '').replace(/\u00a0/g,' ').trim() === t) || ('value' in a && a.value === t);
  }} catch (e) {{}}
  return false;
}})()"#,
                    text_json
                );
                let fallback_ok = handle
                    .evaluate(js.as_str())
                    .await
                    .map(|v| v.value().and_then(|x| x.as_bool()).unwrap_or(false))
                    .unwrap_or(false);
                if !fallback_ok {
                    return Err(error.to_string());
                }
            }
            Ok(())
        })
    }

    fn insert_text_fragment<'a>(
        &'a self,
        page: PageId,
        text: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let handle = self.page(page)?;
            let params = InsertTextParams::new(text.to_string());
            if let Err(error) = handle.execute(params).await {
                let text_json = serde_json::to_string(text).map_err(|e| e.to_string())?;
                let js = format!(
                    r#"(() => {{
  const t = {};
  const a = document.activeElement;
  if (!a) return false;
  if ('value' in a) {{
    const start = typeof a.selectionStart === 'number' ? a.selectionStart : a.value.length;
    const end = typeof a.selectionEnd === 'number' ? a.selectionEnd : start;
    const next = a.value.slice(0, start) + t + a.value.slice(end);
    try {{ a.value = next; }} catch (e) {{ return false; }}
    const caret = start + t.length;
    try {{ if (a.setSelectionRange) a.setSelectionRange(caret, caret); }} catch (e) {{}}
    try {{ a.dispatchEvent(new Event('input', {{bubbles:true}})); }} catch (e) {{}}
    try {{ a.dispatchEvent(new Event('change', {{bubbles:true}})); }} catch (e) {{}}
    return true;
  }}
  if (a.isContentEditable) {{
    a.focus();
    try {{ if (document.execCommand('insertText', false, t)) return true; }} catch (e) {{}}
  }}
  return false;
}})()"#,
                    text_json
                );
                let inserted = handle
                    .evaluate(js.as_str())
                    .await
                    .map(|value| {
                        value
                            .value()
                            .and_then(|value| value.as_bool())
                            .unwrap_or(false)
                    })
                    .unwrap_or(false);
                if !inserted {
                    return Err(error.to_string());
                }
            }
            Ok(())
        })
    }

    fn press_key<'a>(&'a self, page: PageId, key: Key) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let handle = self.page(page)?;
            // A keystroke must never be a session's first input event: dispatch the
            // pointer warm-up once, before the first key leaves the driver.
            self.ensure_warmup(&handle).await;
            let (name, code, vk, text, modifiers) = key_spec(key);
            let mut down = DispatchKeyEventParams::builder()
                .key(name)
                .code(code)
                .windows_virtual_key_code(vk)
                .native_virtual_key_code(vk);
            if modifiers != 0 {
                down = down.modifiers(modifiers);
            }
            // Puppeteer's rule: a key that produces text uses `keyDown` (so the
            // renderer inserts the character); everything else is `rawKeyDown`.
            let down_type = match text {
                Some(value) => {
                    down = down.text(value);
                    DispatchKeyEventType::KeyDown
                }
                None => DispatchKeyEventType::RawKeyDown,
            };

            let mut up = DispatchKeyEventParams::builder()
                .r#type(DispatchKeyEventType::KeyUp)
                .key(name)
                .code(code)
                .windows_virtual_key_code(vk)
                .native_virtual_key_code(vk);
            if modifiers != 0 {
                up = up.modifiers(modifiers);
            }

            // Try CDP key events. If they fail (renderer unstable), fall back to DOM for
            // the common editing keys used by the Gmail flow (SelectAll/Backspace/Delete).
            let cdv = handle
                .execute(
                    down.r#type(down_type)
                        .build()
                        .map_err(|error| error.to_string())?,
                )
                .await;
            if cdv.is_err() {
                // Attempt DOM fallback for editing keys
                let key_name = name;
                let js = match key_name {
                    "a" if (modifiers & 0x04) != 0 || (modifiers & 0x80) != 0 => {
                        // Ctrl+A or Cmd+A -> SelectAll
                        r#"(() => {
  const a = document.activeElement;
  if (!a) return false;
  if ('select' in a) { try { a.select(); return true; } catch (e) {} }
  try { document.execCommand('selectAll', false, null); return true; } catch (e) {}
  return false;
})()"#.to_string()
                    }
                    "Backspace" | "Delete" => {
                        r#"(() => {
  const a = document.activeElement;
  if (!a) return false;
  // Input/textarea
  if ('value' in a) {
    try {
      if (a.selectionStart !== null && a.selectionEnd !== null && a.selectionStart !== a.selectionEnd) {
        a.setRangeText('', a.selectionStart, a.selectionEnd, 'delete');
      } else {
        const start = a.selectionStart ?? a.value.length;
        const end = a.selectionEnd ?? a.selectionStart ?? a.value.length;
        a.setRangeText('', Math.max(0,start-1), end, 'delete');
      }
      try { a.dispatchEvent(new Event('input',{bubbles:true,cancelable:true})); } catch (e) {}
      try { a.dispatchEvent(new Event('change',{bubbles:true})); } catch (e) {}
      return true;
    } catch (e) {}
  }
  // Contenteditable: selectAll first if nothing selected? Or just delete
  try { document.execCommand('selectAll', false, null); document.execCommand('delete', false, null); return true; } catch (e) {}
  try { document.execCommand('delete', false, null); return true; } catch (e) {}
  try { document.execCommand('forwardDelete', false, null); return true; } catch (e) {}
  return false;
})()"#.to_string()
                    }
                    _ => String::new(),
                };
                if !js.is_empty() {
                    let ok = handle
                        .evaluate(js.as_str())
                        .await
                        .map(|v| v.value().and_then(|x| x.as_bool()).unwrap_or(false))
                        .unwrap_or(false);
                    if ok {
                        if let Ok(up_cmd) = up.build() {
                            let _ = handle.execute(up_cmd).await;
                        }
                        return Ok(());
                    }
                }
                cdv.map_err(|e| e.to_string())?;
            }
            handle
                .execute(up.build().map_err(|error| error.to_string())?)
                .await
                .map_err(|error| error.to_string())?;
            Ok(())
        })
    }

    fn focus<'a>(&'a self, page: PageId, selector: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let handle = self.page(page)?;
            let element = handle
                .find_element(selector)
                .await
                .map_err(|error| format!("No element matched {selector:?}: {error}"))?;
            element.focus().await.map_err(|error| error.to_string())?;
            Ok(())
        })
    }

    fn click<'a>(&'a self, page: PageId, selector: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let handle = self.page(page)?;
            let element = handle
                .find_element(selector)
                .await
                .map_err(|error| format!("No element matched {selector:?}: {error}"))?;

            // Human path: approach along a planned path and press at a human point inside the box,
            // holding the button like a finger. Anything that cannot be measured falls back to the
            // element's own click so a click is never silently dropped.
            if self.humanizer.is_some() {
                let _ = element.scroll_into_view().await;
                if let Ok(box_) = element.bounding_box().await {
                    if box_.width > 0.0 && box_.height > 0.0 {
                        let plan = self.humanizer().map(|mut hand| {
                            hand.plan_click_box((box_.x, box_.y, box_.width, box_.height))
                        });
                        if let Some((target, steps, hold)) = plan {
                            dispatch_moves(&handle, &steps).await?;
                            dispatch_press(&handle, target, hold).await?;
                            return Ok(());
                        }
                    }
                }
            }

            element.click().await.map_err(|error| error.to_string())?;
            Ok(())
        })
    }

    fn set_file_input<'a>(
        &'a self,
        page: PageId,
        selector: &'a str,
        files: Vec<PathBuf>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let handle = self.page(page)?;
            let element = handle
                .find_element(selector)
                .await
                .map_err(|error| format!("No file input matched {selector:?}: {error}"))?;
            let params = SetFileInputFilesParams::builder()
                .files(files.iter().map(|path| path.display().to_string()))
                .node_id(element.node_id)
                .build()
                .map_err(|error| error.to_string())?;
            handle
                .execute(params)
                .await
                .map_err(|error| error.to_string())?;
            Ok(())
        })
    }

    fn wait_for_function<'a>(
        &'a self,
        page: PageId,
        script: &'a str,
        timeout_ms: u64,
        poll_ms: u64,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
            let poll = Duration::from_millis(poll_ms.max(1));
            loop {
                match self.evaluate(page, script).await {
                    Ok(value) => {
                        // A predicate yields `false`/`null` while waiting; any
                        // other value (a boolean, a non-empty string, a number)
                        // is treated as success, matching JS truthiness for the
                        // cases the compose flow uses.
                        if !matches!(
                            value,
                            serde_json::Value::Bool(false) | serde_json::Value::Null
                        ) {
                            return Ok(true);
                        }
                    }
                    // A page that navigated mid-poll is not a hard failure: retry
                    // until the deadline so a compose reload can still settle.
                    Err(error) => {
                        log::debug!("[browser] wait_for_function evaluate error: {error}")
                    }
                }
                if std::time::Instant::now() >= deadline {
                    return Ok(false);
                }
                tokio::time::sleep(poll).await;
            }
        })
    }

    fn set_content<'a>(&'a self, page: PageId, html: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let handle = self.page(page)?;
            let path = self.write_html_to_temp(html)?;
            let url = match url::Url::from_file_path(&path) {
                Ok(url) => url.to_string(),
                Err(()) => {
                    let _ = std::fs::remove_file(&path);
                    return Err("Could not build a file URL for the rendered document.".to_string());
                }
            };

            // A navigation that fails outright is a real error; a *timeout* is
            // usually a slow remote image, so keep going and print what is on
            // the page — exactly what desktop runtime's document loader did.
            match tokio::time::timeout(LOAD_TIMEOUT, handle.goto(url)).await {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    let _ = std::fs::remove_file(&path);
                    return Err(format!("Could not render the document: {error}"));
                }
                Err(_) => {
                    log::warn!("[browser] document load timed out; printing the current page")
                }
            }

            evaluate_and_wait(&handle, WAIT_FOR_IMAGES_JS).await;
            evaluate_and_wait(&handle, WAIT_FOR_FONTS_JS).await;
            let _ = std::fs::remove_file(&path);
            Ok(())
        })
    }

    fn evaluate<'a>(
        &'a self,
        page: PageId,
        script: &'a str,
    ) -> BoxFuture<'a, Result<serde_json::Value, String>> {
        Box::pin(async move {
            let handle = self.page(page)?;
            let params = EvaluateParams::builder()
                .expression(script)
                .await_promise(true)
                .build()
                .map_err(|error| error.to_string())?;
            let result = handle
                .evaluate(params)
                .await
                .map_err(|error| error.to_string())?;
            Ok(result.value().cloned().unwrap_or(serde_json::Value::Null))
        })
    }

    fn screenshot<'a>(&'a self, page: PageId) -> BoxFuture<'a, Result<Vec<u8>, String>> {
        Box::pin(async move {
            let handle = self.page(page)?;
            handle
                .screenshot(ScreenshotParams::default())
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn screenshot_full_page<'a>(
        &'a self,
        page: PageId,
    ) -> BoxFuture<'a, Result<Vec<u8>, String>> {
        Box::pin(async move {
            let handle = self.page(page)?;
            let dimensions = handle
                .evaluate(
                    EvaluateParams::builder()
                        .expression("({width: Math.ceil(Math.max(document.documentElement.scrollWidth, document.body?.scrollWidth || 0, innerWidth)), height: Math.ceil(Math.max(document.documentElement.scrollHeight, document.body?.scrollHeight || 0, innerHeight))})")
                        .build()
                        .map_err(|error| error.to_string())?,
                )
                .await
                .map_err(|error| error.to_string())?
                .value()
                .cloned()
                .ok_or_else(|| "Could not read the rendered page dimensions.".to_string())?;
            let width = dimensions
                .get("width")
                .and_then(serde_json::Value::as_f64)
                .filter(|value| value.is_finite() && *value > 0.0)
                .ok_or_else(|| "The rendered page has an invalid width.".to_string())?;
            let height = dimensions
                .get("height")
                .and_then(serde_json::Value::as_f64)
                .filter(|value| value.is_finite() && *value > 0.0)
                .ok_or_else(|| "The rendered page has an invalid height.".to_string())?;
            let clip = Viewport::builder()
                .x(0.0)
                .y(0.0)
                .width(width)
                .height(height)
                .scale(1.0)
                .build()
                .map_err(|error| error.to_string())?;
            let params = ScreenshotParams::builder()
                .clip(clip)
                .capture_beyond_viewport(true)
                .build();
            handle
                .screenshot(params)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn screenshot_jpeg<'a>(
        &'a self,
        page: PageId,
        quality: u8,
    ) -> BoxFuture<'a, Result<Vec<u8>, String>> {
        Box::pin(async move {
            let handle = self.page(page)?;
            let params = ScreenshotParams::builder()
                .format(CaptureScreenshotFormat::Jpeg)
                .quality(quality.min(100))
                .build();
            handle
                .screenshot(params)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn screenshot_jpeg_scaled<'a>(
        &'a self,
        page: PageId,
        quality: u8,
        scale: f64,
    ) -> BoxFuture<'a, Result<Vec<u8>, String>> {
        Box::pin(async move {
            let handle = self.page(page)?;
            let dimensions = handle
                .evaluate(
                    EvaluateParams::builder()
                        .expression("({width: Math.ceil(Math.max(document.documentElement.scrollWidth, document.body?.scrollWidth || 0, innerWidth)), height: Math.ceil(Math.max(document.documentElement.scrollHeight, document.body?.scrollHeight || 0, innerHeight))})")
                        .build()
                        .map_err(|error| error.to_string())?,
                )
                .await
                .map_err(|error| error.to_string())?
                .value()
                .cloned()
                .ok_or_else(|| "Could not read the rendered page dimensions.".to_string())?;
            let width = dimensions
                .get("width")
                .and_then(serde_json::Value::as_f64)
                .filter(|value| value.is_finite() && *value > 0.0)
                .ok_or_else(|| "The rendered page has an invalid width.".to_string())?;
            let height = dimensions
                .get("height")
                .and_then(serde_json::Value::as_f64)
                .filter(|value| value.is_finite() && *value > 0.0)
                .ok_or_else(|| "The rendered page has an invalid height.".to_string())?;
            let clip = Viewport::builder()
                .x(0.0)
                .y(0.0)
                .width(width)
                .height(height)
                .scale(scale.clamp(0.05, 3.0))
                .build()
                .map_err(|error| error.to_string())?;
            let params = ScreenshotParams::builder()
                .format(CaptureScreenshotFormat::Jpeg)
                .quality(quality.min(100))
                .clip(clip)
                .capture_beyond_viewport(true)
                .build();
            handle
                .screenshot(params)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn screenshot_jpeg_region<'a>(
        &'a self,
        page: PageId,
        quality: u8,
        scale: f64,
        width: f64,
        height: f64,
    ) -> BoxFuture<'a, Result<Vec<u8>, String>> {
        Box::pin(async move {
            if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
                return Err("The rendered page has invalid dimensions.".to_string());
            }
            let handle = self.page(page)?;
            let clip = Viewport::builder()
                .x(0.0)
                .y(0.0)
                .width(width)
                .height(height)
                .scale(scale.clamp(0.05, 3.0))
                .build()
                .map_err(|error| error.to_string())?;
            let params = ScreenshotParams::builder()
                .format(CaptureScreenshotFormat::Jpeg)
                .quality(quality.min(100))
                .clip(clip)
                .capture_beyond_viewport(true)
                .build();
            handle
                .screenshot(params)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn emulate_viewport<'a>(
        &'a self,
        page: PageId,
        width: f64,
        height: f64,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let handle = self.page(page)?;
            let params = SetDeviceMetricsOverrideParams::new(
                width.round().clamp(1.0, 10_000_000.0) as i64,
                height.round().clamp(1.0, 10_000_000.0) as i64,
                // Keep the CSS pixel density at 1: the office capture zooms
                // via the screenshot `scale`, not the device metrics.
                1.0,
                false,
            );
            handle
                .execute(params)
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        })
    }

    fn clear_viewport_emulation<'a>(
        &'a self,
        page: PageId,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let handle = self.page(page)?;
            handle
                .execute(ClearDeviceMetricsOverrideParams {})
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        })
    }

    fn print_pdf<'a>(
        &'a self,
        page: PageId,
        options: &'a PdfOptions,
    ) -> BoxFuture<'a, Result<Vec<u8>, String>> {
        Box::pin(async move {
            let handle = self.page(page)?;
            let (width, height) = paper_size_inches(&options.page_size)
                .ok_or_else(|| format!("Unsupported page size \"{}\".", options.page_size))?;
            let params = PrintToPdfParams::builder()
                .landscape(options.landscape)
                .print_background(options.print_background)
                .prefer_css_page_size(options.prefer_css_page_size)
                .paper_width(width)
                .paper_height(height)
                .margin_top(options.margins.top_mm / 25.4)
                .margin_bottom(options.margins.bottom_mm / 25.4)
                .margin_left(options.margins.left_mm / 25.4)
                .margin_right(options.margins.right_mm / 25.4)
                .build();
            tokio::time::timeout(PRINT_TIMEOUT, handle.pdf(params))
                .await
                .map_err(|_| "Timed out printing the document.".to_string())?
                .map_err(|error| error.to_string())
        })
    }

    fn acquire_page<'a>(&'a self, url: Option<&'a str>) -> BoxFuture<'a, Result<PageId, String>> {
        Box::pin(async move {
            let reused = self
                .idle_pages
                .lock()
                .map_err(|error| error.to_string())?
                .pop();
            if let Some(page) = reused {
                // The caller replaces the content with `set_content`, but a
                // stale `about:blank` navigation keeps a failed render from
                // exposing the previous document.
                if let Some(target) = url {
                    if let Ok(handle) = self.page(page) {
                        let _ = handle.goto(target).await;
                    }
                }
                return Ok(page);
            }
            self.new_page(url).await
        })
    }

    fn release_page<'a>(&'a self, page: PageId) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if let Ok(mut idle) = self.idle_pages.lock() {
                if idle.len() < IDLE_PAGE_LIMIT && !idle.contains(&page) {
                    idle.push(page);
                    return Ok(());
                }
            }
            self.close_page(page).await
        })
    }

    fn page_urls<'a>(&'a self) -> BoxFuture<'a, Result<Vec<(PageId, String)>, String>> {
        Box::pin(async move {
            let handles = {
                let browser = self.browser.lock().await;
                browser.pages().await.map_err(|error| error.to_string())?
            };
            let mut out = Vec::with_capacity(handles.len());
            for handle in handles {
                let url = handle.url().await.ok().flatten().unwrap_or_default();
                let id = {
                    let mut known = self.pages.lock().map_err(|error| error.to_string())?;
                    let existing = known
                        .iter()
                        .find(|(_, page)| page.target_id() == handle.target_id())
                        .map(|(id, _)| *id);
                    match existing {
                        Some(id) => id,
                        None => {
                            // A target the driver did not create (Gmail's
                            // separate compose window, a popup, the launch
                            // tab). Adopt it so callers can inspect or drive it.
                            let id = self.next_page.fetch_add(1, Ordering::Relaxed);
                            known.insert(id, handle);
                            id
                        }
                    }
                };
                out.push((id, url));
            }
            Ok(out)
        })
    }

    fn close_page<'a>(&'a self, page: PageId) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let handle = self
                .pages
                .lock()
                .map_err(|error| error.to_string())?
                .remove(&page);
            if let Some(handle) = handle {
                let _ = handle.close().await;
            }
            Ok(())
        })
    }

    fn watch_javascript_dialogs<'a>(
        &'a self,
        page: PageId,
        needles: &'a [&'a str],
    ) -> BoxFuture<'a, Result<DialogWatcher, String>> {
        Box::pin(async move {
            let handle = self.page(page)?;
            // Lowercased once so the per-dialog check does no extra allocation.
            let needles: Vec<String> = needles
                .iter()
                .map(|needle| needle.to_ascii_lowercase())
                .collect();
            // `Page` is enabled by chromiumoxide when the target attaches, so
            // `Page.javascriptDialogOpening` is delivered without any extra
            // setup. Registering the listener before the dialog can open is what
            // makes the event observable: chromiumoxide does not replay events
            // that fired before a listener existed.
            let mut dialogs = handle
                .event_listener::<EventJavascriptDialogOpening>()
                .await
                .map_err(|error| format!("Could not watch for Gmail's dialogs: {error}"))?;
            let watcher_page = handle.clone();
            let task = tokio::spawn(async move {
                while let Some(event) = dialogs.next().await {
                    let matched = {
                        let message = event.message.to_ascii_lowercase();
                        needles
                            .iter()
                            .any(|needle| message.contains(needle.as_str()))
                    };
                    if matched {
                        log::info!(
                            "[browser] accepting Gmail's send dialog: {}",
                            event.message
                        );
                    } else {
                        log::warn!(
                            "[browser] accepting an unexpected JavaScript dialog: {}",
                            event.message
                        );
                    }
                    // Accepting releases the renderer's blocked JavaScript;
                    // dismissing a confirm would cancel the send instead.
                    if let Err(error) = watcher_page
                        .execute(HandleJavaScriptDialogParams::new(true))
                        .await
                    {
                        log::debug!("[browser] JavaScript dialog was already closed: {error}");
                    }
                }
            });
            Ok(DialogWatcher::new(task))
        })
    }

    fn shutdown<'a>(&'a self) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            {
                let mut browser = self.browser.lock().await;
                if let Err(error) = browser.close().await {
                    log::debug!("[browser] close returned an error: {error}");
                }
                let _ = browser.wait().await;
            }
            if let Ok(mut handler) = self.handler.lock() {
                if let Some(join) = handler.take() {
                    join.abort();
                }
            }
            if let Ok(mut pages) = self.pages.lock() {
                pages.clear();
            }
            if let Ok(mut idle) = self.idle_pages.lock() {
                idle.clear();
            }
            if let Some(dir) = &self.profile_dir {
                let _ = std::fs::remove_dir_all(dir);
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comma_separated_addresses_bypass_typing() {
        assert!(looks_like_recipient_list("a@x.test, b@y.test"));
        assert!(looks_like_recipient_list("a@x.test,b@y.test,c@z.test"));
        // Whitespace-only padding still counts as a list.
        assert!(looks_like_recipient_list("  a@x.test ,  b@y.test "));
    }

    #[test]
    fn comma_text_that_is_not_a_recipient_list_stays_typed() {
        // Subjects/body lines lose realism if they take the atomic path, so a
        // comma without all-address parts keeps the keystrokes.
        assert!(!looks_like_recipient_list("Hello, world"));
        assert!(!looks_like_recipient_list("One, two, three"));
        // Partly non-addressed: a name list, not a recipient list.
        assert!(!looks_like_recipient_list("a@x.test, b, c@z.test"));
    }

    #[test]
    fn single_fields_always_stay_typed() {
        assert!(!looks_like_recipient_list(""));
        assert!(!looks_like_recipient_list("a@x.test"));
        assert!(!looks_like_recipient_list("Nova@Novamail.io"));
    }

    #[test]
    fn bare_addresses_take_the_atomic_path() {
        // Exactly the production To-field shape: one address per message.
        assert!(looks_like_recipient_address("Nova@Novamail.io"));
        assert!(looks_like_recipient_address("a@x.test"));
        // Surrounding whitespace is field padding, not content.
        assert!(looks_like_recipient_address("  user@example.com "));
    }

    #[test]
    fn address_shaped_gate_keeps_prose_typable() {
        // Nothing here is a bare address (missing dot, spaces, no `@`, two `@`),
        // so subjects and bodies keep the keystroke realism.
        assert!(!looks_like_recipient_address(""));
        assert!(!looks_like_recipient_address("Nova@Novamail"));
        assert!(!looks_like_recipient_address("Ask services@site.com"));
        assert!(!looks_like_recipient_address("reply me"));
        assert!(!looks_like_recipient_address("a@@b.org"));
        // A quoted address inside a body line is prose, not a recipient.
        assert!(!looks_like_recipient_address("contact: help@x.test, please"));
    }
}
