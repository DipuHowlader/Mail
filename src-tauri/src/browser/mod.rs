//! Browser automation driver.
//!
//! Ports the browser half of `desktop-runtime/documentEngine.cjs`: the app drives the
//! vendored Clearcote Chromium over the Chrome DevTools Protocol instead of
//! going through the Clearcote/Playwright launcher. Everything above this layer
//! talks to a [`BrowserDriver`] trait object so the concrete engine can be
//! swapped (chromiumoxide today, WebView2 or a sidecar later) without touching
//! the document engine or the automation queue.
//!
//! The browser is launched lazily: nothing is spawned until the first document
//! export actually needs a page.

//! The browser is launched lazily: nothing is spawned until the first document
//! export actually needs a page.

pub mod chromium;
pub mod humanize;
pub mod paths;

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use serde_json::Value;

/// A boxed, `Send` future — the object-safe stand-in for `async fn` in a trait.
///
/// `async fn` in traits is stable but not `dyn`-compatible, and the migration's
/// locked decision is a swappable `BrowserDriver` trait object.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Handle to a page owned by a driver.
pub type PageId = u64;

/// A keystroke the automation layer dispatches.
///
/// Only the keys the Gmail compose flow needs are modelled. The `Ctrl*`
/// variants use Command instead of Control on macOS, exactly like
/// `sendOneCore` in `desktop-runtime/automation.cjs` (which keyed off
/// `process.platform === 'darwin'`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// Commit a recipient chip / confirm a focused control.
    Enter,
    /// Delete the current selection (`Backspace`).
    Backspace,
    /// Dismiss a dialog (`Escape`). Modelled for the compose flow but not dispatched yet — the
    /// current flow closes dialogs through the DOM.
    #[allow(dead_code)]
    Escape,
    /// Select the whole field (`Ctrl+A` / `Cmd+A`).
    SelectAll,
    /// Paste the OS clipboard (`Ctrl+V` / `Cmd+V`).
    Paste,
    /// Gmail's send shortcut (`Ctrl+Enter` / `Cmd+Enter`). Superseded by the DOM send click, kept
    /// so the key map stays complete.
    #[allow(dead_code)]
    Submit,
}

/// Everything needed to start a browser.
#[derive(Clone, Debug)]
pub struct DriverConfig {
    pub executable: PathBuf,
    /// `true` runs the browser without a visible window (document rendering).
    pub headless: bool,
    /// Throwaway or persistent profile directory.
    pub user_data_dir: Option<PathBuf>,
    pub window_size: Option<(u32, u32)>,
    /// Extra Chromium flags, appended after the driver's own defaults.
    pub extra_args: Vec<String>,
    /// Drive the pointer and keyboard through the human-motion model
    /// ([`humanize`]) instead of instantaneous synthetic events.
    pub humanize: bool,
    /// Identity seed for the motor persona. `None` draws a fresh hand per launch.
    pub humanize_seed: Option<String>,
    /// Keep `user_data_dir` on shutdown instead of deleting it. Used by the live
    /// diagnostic so a signed-in session survives between runs.
    pub keep_profile: bool,
}

impl DriverConfig {
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
            headless: true,
            user_data_dir: None,
            window_size: None,
            extra_args: Vec::new(),
            humanize: false,
            humanize_seed: None,
            keep_profile: false,
        }
    }

    pub fn headless(mut self, value: bool) -> Self {
        self.headless = value;
        self
    }

    pub fn user_data_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.user_data_dir = Some(dir.into());
        self
    }

    pub fn window_size(mut self, width: u32, height: u32) -> Self {
        self.window_size = Some((width, height));
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        for arg in args {
            self.extra_args.push(arg.into());
        }
        self
    }

    /// Enable human-motion pointer/keyboard input, seeded by `seed`.
    pub fn humanize(mut self, seed: Option<impl Into<String>>) -> Self {
        self.humanize = true;
        self.humanize_seed = seed.map(Into::into);
        self
    }

    /// Keep `user_data_dir` on shutdown instead of deleting it (live diagnostics
    /// need a session that survives between runs).
    pub fn keep_profile(mut self, value: bool) -> Self {
        self.keep_profile = value;
        self
    }
}

/// Page margins, in millimetres like desktop runtime's `page.pdf()` options.
#[derive(Clone, Debug)]
pub struct Margins {
    pub top_mm: f64,
    pub bottom_mm: f64,
    pub left_mm: f64,
    pub right_mm: f64,
}

impl Margins {
    /// Uniform margins on all four sides. Kept for callers that want symmetric
    /// output; the document defaults are asymmetric.
    #[allow(dead_code)]
    pub const fn all(value: f64) -> Self {
        Self {
            top_mm: value,
            bottom_mm: value,
            left_mm: value,
            right_mm: value,
        }
    }
}

/// Print settings, mirroring `renderPdf()` in `desktop-runtime/documentEngine.cjs`.
#[derive(Clone, Debug)]
pub struct PdfOptions {
    /// Named paper size: `A4`, `Letter`, `Legal`, `A3`, `A5`, `A6`, `Tabloid`, `Ledger`.
    pub page_size: String,
    pub landscape: bool,
    pub print_background: bool,
    pub prefer_css_page_size: bool,
    pub margins: Margins,
}

impl Default for PdfOptions {
    fn default() -> Self {
        Self {
            page_size: "A4".to_string(),
            landscape: false,
            print_background: true,
            prefer_css_page_size: true,
            margins: Margins {
                top_mm: 15.0,
                bottom_mm: 15.0,
                left_mm: 12.0,
                right_mm: 12.0,
            },
        }
    }
}

/// Named paper sizes in inches (the unit CDP `Page.printToPDF` expects).
pub fn paper_size_inches(name: &str) -> Option<(f64, f64)> {
    match name.trim().to_ascii_uppercase().as_str() {
        "A3" => Some((11.69, 16.54)),
        "A4" => Some((8.27, 11.69)),
        "A5" => Some((5.83, 8.27)),
        "A6" => Some((4.13, 5.83)),
        "LETTER" => Some((8.5, 11.0)),
        "LEGAL" => Some((8.5, 14.0)),
        "TABLOID" => Some((11.0, 17.0)),
        "LEDGER" => Some((17.0, 11.0)),
        _ => None,
    }
}

/// Keeps a driver's native-JavaScript-dialog watcher alive; dropping the guard
/// stops it.
///
/// Gmail raises a *native* `confirm()` — "Send this message without a subject or
/// text in the body?" — when a message is empty. A native dialog is drawn by the
/// browser chrome, not the page DOM, so no injected script can see it or click
/// its buttons, and it **blocks the renderer's JavaScript** until it is answered.
/// The Chromium driver watches `Page.javascriptDialogOpening` over CDP and
/// accepts the dialog; holding this guard keeps that watcher running for the
/// duration of a send.
pub struct DialogWatcher {
    /// `None` for drivers that cannot observe native dialogs.
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl DialogWatcher {
    /// Wrap a running watcher task. Used by the Chromium driver.
    pub(crate) fn new(handle: tokio::task::JoinHandle<()>) -> Self {
        Self {
            handle: Some(handle),
        }
    }

    /// A guard that does nothing, for drivers without native-dialog support.
    pub(crate) fn disabled() -> Self {
        Self { handle: None }
    }
}

impl Drop for DialogWatcher {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

/// The browser operations the app needs. Object-safe by design: methods return
/// a boxed, `Send` future instead of using `async fn` (which is not `dyn`-safe).
pub trait BrowserDriver: Send + Sync {
    /// Short identifier for diagnostics and telemetry.
    fn engine(&self) -> &'static str;

    /// Whether the browser process has exited independently of an app request.
    /// Drivers without an exit signal remain open until explicitly shut down.
    fn is_browser_closed(&self) -> bool {
        false
    }

    /// Open a new page, optionally navigating to `url` first.
    fn new_page<'a>(&'a self, url: Option<&'a str>) -> BoxFuture<'a, Result<PageId, String>>;

    /// Navigate an existing page. Used by the automation layer (Phase 4); it is
    /// part of the trait surface so callers never depend on chromiumoxide.
    #[allow(dead_code)]
    fn goto<'a>(&'a self, page: PageId, url: &'a str) -> BoxFuture<'a, Result<(), String>>;

    /// Insert `text` at the focused element.
    ///
    /// Driver behaviour with humanization enabled: short text (≤
    /// [`HUMAN_TYPE_MAX_CHARS`], and not a comma-separated recipient list) is typed
    /// key-by-key through the persona hand so keystroke dynamics are human; the value is
    /// verified afterwards and anything else — long bodies, recipient lists, unverified
    /// attempts — falls back to the single atomic `Input.insertText` mirroring
    /// Playwright's `keyboard.insertText`, exactly as older builds did. Drivers without
    /// humanization always take the atomic path.
    fn insert_text<'a>(&'a self, page: PageId, text: &'a str) -> BoxFuture<'a, Result<(), String>>;

    /// Insert a small text fragment at the current caret without replacing the
    /// field contents. Drivers may override this for reliable character-wise
    /// input; the default keeps lightweight test drivers compatible.
    fn insert_text_fragment<'a>(
        &'a self,
        page: PageId,
        text: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.insert_text(page, text)
    }

    /// Dispatch one keystroke (or Ctrl/Cmd shortcut) to the page's focused
    /// element.
    fn press_key<'a>(&'a self, page: PageId, key: Key) -> BoxFuture<'a, Result<(), String>>;

    /// Focus the first element matching `selector`. Retained on the driver API for flows that need a
    /// real focus (the current Gmail flow focuses via injected scripts).
    #[allow(dead_code)]
    fn focus<'a>(&'a self, page: PageId, selector: &'a str) -> BoxFuture<'a, Result<(), String>>;

    /// Click the first element matching `selector` with a real mouse event
    /// (scroll into view, then press and release at the element's centre). The humanised pointer
    /// path lives here; the current Gmail flow deliberately uses a DOM `element.click()`.
    #[allow(dead_code)]
    fn click<'a>(&'a self, page: PageId, selector: &'a str) -> BoxFuture<'a, Result<(), String>>;

    /// Attach `files` to the first `input[type=file]` matching `selector`,
    /// equivalent to Playwright's `elementHandle.setInputFiles`.
    fn set_file_input<'a>(
        &'a self,
        page: PageId,
        selector: &'a str,
        files: Vec<std::path::PathBuf>,
    ) -> BoxFuture<'a, Result<(), String>>;

    /// Poll `script` (an expression that yields a boolean, or a promise of one)
    /// until it is truthy or `timeout_ms` elapses. Returns the final truthiness
    /// so the caller decides whether a timeout is fatal.
    fn wait_for_function<'a>(
        &'a self,
        page: PageId,
        script: &'a str,
        timeout_ms: u64,
        poll_ms: u64,
    ) -> BoxFuture<'a, Result<bool, String>>;

    /// Render `html` into the page.
    fn set_content<'a>(&'a self, page: PageId, html: &'a str) -> BoxFuture<'a, Result<(), String>>;

    /// Evaluate a JS expression and return its value.
    fn evaluate<'a>(
        &'a self,
        page: PageId,
        script: &'a str,
    ) -> BoxFuture<'a, Result<Value, String>>;

    /// Capture a PNG screenshot of the page.
    fn screenshot<'a>(&'a self, page: PageId) -> BoxFuture<'a, Result<Vec<u8>, String>>;

    /// Capture the complete rendered document, including content beyond the viewport.
    fn screenshot_full_page<'a>(
        &'a self,
        page: PageId,
    ) -> BoxFuture<'a, Result<Vec<u8>, String>> {
        self.screenshot(page)
    }

    /// Capture a JPEG screenshot for image-backed Office documents. Drivers
    /// without quality control can keep their normal screenshot behavior.
    fn screenshot_jpeg<'a>(
        &'a self,
        page: PageId,
        _quality: u8,
    ) -> BoxFuture<'a, Result<Vec<u8>, String>> {
        self.screenshot(page)
    }

    /// Capture the whole rendered document as a JPEG, scaling the output
    /// without reflowing its CSS layout. Values above 1.0 re-rasterize the
    /// page at a higher device-pixel density (zoom, not reflow), which Office
    /// documents need to stay as crisp as the vector PDF. Drivers without CDP
    /// support may use a normal JPEG screenshot as a fallback.
    fn screenshot_jpeg_scaled<'a>(
        &'a self,
        page: PageId,
        quality: u8,
        _scale: f64,
    ) -> BoxFuture<'a, Result<Vec<u8>, String>> {
        self.screenshot_jpeg(page, quality)
    }

    /// Reflow the page at an emulated viewport width before content is
    /// loaded. The document engine needs this because the PDF reflows at the
    /// paper width, while an un-emulated page renders at whatever the browser
    /// window happens to be — a centered template then leaves wide empty
    /// sides in the Office captures. Drivers without CDP support ignore the
    /// request (the layout then simply uses the window width).
    fn emulate_viewport<'a>(
        &'a self,
        _page: PageId,
        _width: f64,
        _height: f64,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    /// Remove any viewport emulation installed by [`emulate_viewport`] so a
    /// pooled page renders at its real window size again.
    fn clear_viewport_emulation<'a>(
        &'a self,
        _page: PageId,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    /// Like [`screenshot_jpeg_scaled`], but with an explicit clip size so the
    /// caller can reuse dimensions it already measured (the Office ladder would
    /// otherwise re-measure the page for every attempt). Drivers without CDP
    /// support ignore the size and fall back to `screenshot_jpeg_scaled`.
    fn screenshot_jpeg_region<'a>(
        &'a self,
        page: PageId,
        quality: u8,
        scale: f64,
        _width: f64,
        _height: f64,
    ) -> BoxFuture<'a, Result<Vec<u8>, String>> {
        self.screenshot_jpeg_scaled(page, quality, scale)
    }

    /// Print the page to PDF.
    fn print_pdf<'a>(
        &'a self,
        page: PageId,
        options: &'a PdfOptions,
    ) -> BoxFuture<'a, Result<Vec<u8>, String>>;

    /// Close a single page.
    fn close_page<'a>(&'a self, page: PageId) -> BoxFuture<'a, Result<(), String>>;

    /// Acquire a page for a one-shot render, reusing an idle page when the
    /// driver pools them. Reusing a warm page avoids the per-conversion cost of
    /// creating and tearing down a CDP target; drivers without a pool just open
    /// a new page. Every `acquire_page` must be paired with [`release_page`].
    fn acquire_page<'a>(&'a self, url: Option<&'a str>) -> BoxFuture<'a, Result<PageId, String>> {
        self.new_page(url)
    }

    /// Return a page acquired with [`acquire_page`] so the driver can reuse it.
    fn release_page<'a>(&'a self, page: PageId) -> BoxFuture<'a, Result<(), String>> {
        self.close_page(page)
    }

    /// Every open page with its current URL, in the driver's own [`PageId`]s.
    ///
    /// Used to diagnose a Gmail flow that opened its editor in a different
    /// target than the one the caller drives (for example when an account is
    /// set to "Compose in a new window"). Drivers that do not track their
    /// targets return an empty list.
    fn page_urls<'a>(&'a self) -> BoxFuture<'a, Result<Vec<(PageId, String)>, String>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    /// Start accepting Gmail's native "send without a subject/body" confirm
    /// dialog on `page`, and return a guard that stops watching when dropped.
    ///
    /// A native dialog blocks the renderer's JavaScript until it is answered, so
    /// an unanswered prompt would stall the send and every later compose. The
    /// driver therefore accepts every dialog it sees (clicking its OK/"Send
    /// anyway" button); `needles` are lowercase substrings identifying the prompt
    /// we expect, and only decide whether it is logged at info or warn level.
    ///
    /// Drivers that cannot observe native dialogs return a no-op guard.
    fn watch_javascript_dialogs<'a>(
        &'a self,
        _page: PageId,
        _needles: &'a [&'a str],
    ) -> BoxFuture<'a, Result<DialogWatcher, String>> {
        Box::pin(async { Ok(DialogWatcher::disabled()) })
    }

    /// Close the browser and every page it owns.
    fn shutdown<'a>(&'a self) -> BoxFuture<'a, Result<(), String>>;
}

/// Lazily launches and owns the driver for this process.
///
/// The launcher window never calls a document/automation command, so its
/// `BrowserService` is never turned into a running browser.
pub struct BrowserService {
    driver: tokio::sync::Mutex<Option<std::sync::Arc<dyn BrowserDriver>>>,
}

impl Default for BrowserService {
    fn default() -> Self {
        Self {
            driver: tokio::sync::Mutex::new(None),
        }
    }
}

impl BrowserService {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-warm the browser in the background so the first conversion is instant.
    /// Called once during app setup; failures are logged but not fatal.
    pub async fn prewarm(&self) {
        let config = match default_render_config() {
            Ok(config) => config,
            Err(error) => {
                log::warn!("[browser] prewarm config failed: {error}");
                return;
            }
        };
        let driver: std::sync::Arc<dyn BrowserDriver> =
            match chromium::ChromiumDriver::launch(config).await {
                Ok(driver) => std::sync::Arc::new(driver),
                Err(error) => {
                    log::warn!("[browser] prewarm launch failed: {error}");
                    return;
                }
            };
        let mut guard = self.driver.lock().await;
        *guard = Some(driver);
        log::info!("[browser] pre-warmed and ready");
    }

    /// Return the running driver, launching Chromium on first use.
    ///
    /// The launch lock is held for the whole (expensive) start-up so concurrent
    /// callers start exactly one browser.
    pub async fn driver(&self) -> Result<std::sync::Arc<dyn BrowserDriver>, String> {
        let mut guard = self.driver.lock().await;
        if let Some(driver) = guard.as_ref() {
            return Ok(driver.clone());
        }
        let config = default_render_config()?;
        let driver: std::sync::Arc<dyn BrowserDriver> =
            std::sync::Arc::new(chromium::ChromiumDriver::launch(config).await?);
        *guard = Some(driver.clone());
        Ok(driver)
    }

    /// `true` once a browser is running. Read by telemetry (Phase 4).
    #[allow(dead_code)]
    pub async fn is_running(&self) -> bool {
        self.driver.lock().await.is_some()
    }

    /// Close the browser, if one was started.
    pub async fn shutdown(&self) {
        let driver = { self.driver.lock().await.take() };
        if let Some(driver) = driver {
            let _ = driver.shutdown().await;
        }
    }
}

/// The config used for document rendering: headless, throwaway profile.
pub fn default_render_config() -> Result<DriverConfig, String> {
    let executable = paths::chromium_executable_path()?;
    Ok(DriverConfig::new(executable)
        .headless(true)
        .user_data_dir(paths::default_profile_dir())
        .window_size(1280, 720)
        .args(paths::RENDER_BROWSER_ARGS.iter().map(|arg| arg.to_string())))
}
