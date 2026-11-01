//! Locate the Chromium runtime the app drives.
//!
//! Ports `desktop-runtime/chromiumPath.cjs`. The runtime is vendored under
//! `vendor/clearcote/browser` in a checkout and copied to `<resources>/clearcote`
//! when packaged, so the app never has to download a browser. A
//! `NOVAMAIL_CHROMIUM_PATH` override wins over the bundled binary, and an
//! installed Chrome/Edge is the last resort (handy on a dev box where the
//! vendor folder has not been restored yet).

use std::path::PathBuf;

/// Extra flags for the shared render browser, mirroring `ensureRenderBrowser()`
/// in `desktop-runtime/documentEngine.cjs`. Keeping background pages unthrottled is
/// what stops Chromium from slowing timers in the render/automation windows.
///
/// Also includes aggressive anti-detection flags so the automation layer does not
/// reveal itself through `navigator.webdriver`, missing plugins, or other
/// automation fingerprints.
///
/// Memory-saving flags are critical when multiple apps each run their own
/// Chromium — without them a single instance can consume 400-600 MB and
/// 4-5 concurrent instances will exhaust system RAM.
pub const RENDER_BROWSER_ARGS: &[&str] = &[
    // --- Performance / unthrottled rendering ---
    "--disable-background-timer-throttling",
    "--disable-backgrounding-occluded-windows",
    "--disable-renderer-backgrounding",
    "--disable-ipc-flooding-protection",
    // --- Memory savings ---
    // No GPU process: saves ~50-100 MB per instance. Document rendering
    // uses software rasterization anyway.
    "--disable-gpu",
    // No background component updates (widevine, component updater).
    "--disable-component-update",
    // No domain reliability monitoring (background pings + storage).
    "--disable-domain-reliability",
    // Cap renderer processes at 2. The default is ~30; document
    // rendering needs at most 1.
    "--renderer-process-limit=2",
    // Cap V8 heap at 512 MB per renderer. The default is system-
    // dependent (often 2-4 GB) and a hungry renderer can exhaust RAM.
    "--js-flags=--max-old-space-size=512",
    // Don't autorun the utility services (network, storage, etc.).
    "--no-service-autorun",
    // --- Anti-detection: hide automation fingerprints ---
    "--disable-blink-features=AutomationControlled",
    "--disable-features=IsolateOrigins,site-per-process",
    "--no-default-browser-check",
    "--no-first-run",
    "--disable-infobars",
    "--disable-extensions",
    "--disable-popup-blocking",
    "--disable-session-crashed-bubble",
    "--test-type",
    // --- Anti-detection: make the browser look like a real user ---
    "--window-size=1920,1080",
    "--start-maximized",
    "--disable-features=Translate",
    "--disable-features=OptimizationHints",
    "--disable-features=MediaRouter",
    "--disable-features=InterestFeedContentSuggestions",
    "--disable-features=CalculateNativeWinOcclusion",
    // --- Anti-detection: suppress the "Chrome is being controlled" banner ---
    "--disable-features=SuppressUnsupportedFlagWarning",
];

fn executable_name() -> &'static str {
    if cfg!(windows) {
        "chrome.exe"
    } else {
        "chrome"
    }
}

/// Where the vendored Clearcote runtime lives in a dev checkout.
fn bundled_dev_candidate() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("vendor")
        .join("clearcote")
        .join("browser")
        .join(executable_name())
}

/// Layouts the packaged app may use, relative to the running executable.
fn packaged_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let name = executable_name();
            candidates.push(dir.join("clearcote").join("browser").join(name));
            candidates.push(
                dir.join("resources")
                    .join("clearcote")
                    .join("browser")
                    .join(name),
            );
            // macOS `.app` bundle: Contents/MacOS/<exe> → Contents/Resources.
            candidates.push(
                dir.join("..")
                    .join("Resources")
                    .join("clearcote")
                    .join("browser")
                    .join(name),
            );
        }
    }
    candidates
}

/// Installed Chrome / Edge, used only when the bundled runtime is missing.
#[cfg(windows)]
fn installed_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    for var in ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"] {
        if let Ok(base) = std::env::var(var) {
            candidates.push(
                PathBuf::from(&base)
                    .join("Google")
                    .join("Chrome")
                    .join("Application")
                    .join("chrome.exe"),
            );
        }
    }
    for var in ["PROGRAMFILES(X86)", "PROGRAMFILES", "LOCALAPPDATA"] {
        if let Ok(base) = std::env::var(var) {
            candidates.push(
                PathBuf::from(&base)
                    .join("Microsoft")
                    .join("Edge")
                    .join("Application")
                    .join("msedge.exe"),
            );
        }
    }
    candidates
}

#[cfg(not(windows))]
fn installed_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    for path in [
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    ] {
        candidates.push(PathBuf::from(path));
    }
    candidates
}

/// Every path that will be tried, in order, for the Chromium executable.
pub fn chromium_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(over) = std::env::var("NOVAMAIL_CHROMIUM_PATH") {
        let over = over.trim();
        if !over.is_empty() {
            candidates.push(PathBuf::from(over));
        }
    }
    candidates.push(bundled_dev_candidate());
    candidates.extend(packaged_candidates());
    candidates.extend(installed_candidates());
    candidates
}

/// Resolve the Chromium executable, or explain every path that was tried.
pub fn chromium_executable_path() -> Result<PathBuf, String> {
    let candidates = chromium_candidates();
    if let Some(found) = candidates.iter().find(|path| path.is_file()) {
        return Ok(found.clone());
    }
    let tried = candidates
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "The bundled Clearcote Chromium was not found. Reinstall Novamail or restore the \
         vendor/clearcote folder (searched: {tried})."
    ))
}

/// A fresh, per-process throwaway profile directory.
///
/// Each app process gets its own directory so two apps never share a Chromium
/// profile, and the render browser is not polluted by a previous run.
pub fn default_profile_dir() -> PathBuf {
    std::env::temp_dir().join("novamail-browser").join(format!(
        "profile-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ))
}
