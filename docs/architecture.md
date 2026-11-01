# Novamail architecture: Tauri launcher and isolated app processes

Novamail's native desktop path uses one Tauri executable with two process roles:

| Role | Entry point | Responsibility |
| --- | --- | --- |
| Launcher | `src-tauri/src/lib.rs` with the default role | Lists, creates, opens, closes, restarts, removes, renames and focuses app instances. It does not own Gmail automation or document rendering. |
| App instance | `--novamail-role=app --novamail-instance=<id>` | Owns one React window, its automation queue and telemetry, its Gmail browser sessions, and document conversion. |

The React UI continues to call `window.api`. `src/services/tauriBridge.js` maps that interface to Tauri commands and events, so the components do not depend on Rust module names or platform APIs.

## Process and browser isolation

The launcher starts another copy of the Tauri executable for each app. Each process has a stable instance id and name. On Windows the launcher requests `CREATE_BREAKAWAY_FROM_JOB` so app processes can survive a launcher crash; if the host policy refuses breakaway, it falls back to a normal child process and logs that crash isolation may be limited.

Each launcher and app process has a separate WebView2 data directory under the Tauri app-data folder. Each automation browser session also launches its own Chromium process with a distinct profile directory. Browser close tears down that session's browser and temporary profile. The launcher and app processes therefore do not share live queues, Gmail browser state or renderer state.

Windows assigns `com.ippo.novamail.launcher` to the launcher and a per-instance AppUserModelID to each app, keeping taskbar grouping separate.

## Launcher control and recovery

The launcher binds a loopback TCP server to an ephemeral port and stores the current port and random token in `launcher-control.json`. It gives the endpoint to every app it starts. Apps send a `hello`, a ready state and heartbeats, and listen for shutdown, focus and rename messages.

If the launcher exits, the app keeps running. On restart, it writes a fresh endpoint; existing apps reread the endpoint and reconnect. The launcher can then focus or gracefully close an adopted app even though it did not create the original process handle. On Windows, forced cleanup uses `taskkill /T /F` to remove the app and its WebView2 children.

The app registry is stored in `instances.json`. It records instance ids, names, account emails, PIDs and last state. The launcher reports a live but not-yet-reconnected process as orphaned and avoids starting a duplicate.

## Shared config store

The app and launcher share `%APPDATA%/Novamail Command/novamail-command.json` to keep licenses and campaigns available across app instances. The Rust store preserves unknown keys for compatibility with existing data. Writers take an OS file lock, reload the latest JSON, then atomically replace the file from a flushed sibling temp file. Campaign updates use one read-modify-write transaction, and reads that need fresh data refresh from disk.

The Tauri license key is stored under `licenseNative` using Windows DPAPI. Tauri's old `safeStorage` value could not be decrypted with the available entropy, so a user entering Tauri for the first time must activate once; subsequent Tauri starts reuse the native value.

## Automation and document output

`src-tauri/src/automation/` owns the queue, retry and pause state, run records and telemetry. A `BrowserDriver` trait keeps that work independent from the CDP implementation. `chromiumoxide` drives the bundled Clearcote Chromium for Gmail and document rendering.

The sender uses Gmail's compose UI, validates the recipient chip and attachments, inserts sanitized rich HTML, and waits for Gmail's sent confirmation. The body is delivered first through an in-page paste event whose `clipboardData` is built inside the browser, which never touches the OS clipboard; if that pipeline is not confirmed within its short window, the sender falls back to the Windows clipboard paste described below. If Gmail's result is ambiguous after clicking Send, the worker pauses that recipient and reports that the Sent folder must be checked before retrying.

The clipboard swap is a short lease used only as the fallback body pipeline: a per-session named mutex serializes install and restore across every app process the launcher starts, the payload carries the browser/password-manager exclusion formats (`CanIncludeInClipboardHistory`, `CanUploadToCloudClipboard`, `ExcludeClipboardContentFromMonitorProcessing`) so transient email bodies never enter Win+V history or the cloud clipboard, and the restore re-checks the clipboard sequence number and retries while Windows still reports the clipboard busy. An operator copy made while an email is mid-paste always wins.

`src-tauri/src/documents.rs` renders the shared HTML in Chromium, returns a PNG preview, and generates PDF, text and inline output. DOCX, XLSX and PPTX contain the rendered page rasterized into a JPEG — no extracted text — sized with correct pixel→EMU conversion (96 dpi) and packaged with complete PowerPoint/Word/Excel structures. All three office formats mirror the PDF page paper (`PaperLayout`, built from the same `pdfOptions` the PDF printer consumes): the same named paper size and landscape orientation; PPTX also keeps the PDF's exact margins, while DOCX and XLSX use slimmer borders (6 mm top/bottom, 5 mm sides) so their content fills more of the same sheet and reads slightly larger than the PDF. The capture reflows at the PDF content-box width in CSS pixels (`Emulation.setDeviceMetricsOverride` — 1 CSS px ≙ 9525 EMU of that box), so templates render at the paper width instead of the browser window's, and `template::responsive_container_html` converts fixed-width main containers to the `width:100%; max-width:1000px` pattern the email path uses too (the outgoing body re-fluidifies container widths after computed-style inlining via `clamp_fluid_inline_widths`); the PDF text/branch keeps the raw template so its output stays byte-stable. The picture is laid out in the same content box the PDF prints into — full printable width and height, not a small centered page. PPTX slides are paper-sized with the picture at the margin offsets; DOCX pages use dynamic `pgSz`/`pgMar` twips; XLSX anchors the picture with the PDF content-box extent. The PowerPoint package additionally carries the python-pptx-grade skeleton (theme, presProps, viewProps, tableStyles, master with background) from `src-tauri/src/office_parts.rs`. To reach the 100 KB generated-attachment ceiling without the blur that caused, the office JPEG ladder starts from a ~300 dpi zoom capture and backs the resolution off in small steps while JPEG quality never drops below 65; Gmail's absolute 25 MB attachment limit remains the hard stop.

The native build bundles `vendor/clearcote` into the NSIS resources and configures Tauri's WebView2 bootstrapper for Windows installs. The installer remains unsigned until a signing identity is configured.

## Benchmark and launcher coverage

Tauri equivalents are available for launcher rendering, registry persistence, process isolation and launcher crash survival:

- `npm run validate:launcher:tauri`
- `npm run validate:launcher-registry:tauri`
- `npm run test:isolation:tauri`
- `npm run test:launcher-crash:tauri`

Native performance scripts are available after building the Tauri app:

- `npm run benchmark:document:tauri -- 1 2 3 4 5`
- `npm run benchmark:browser:tauri -- 1 2 3 4 5`

They report Tauri/Chromium timings and process cleanup. No new Tauri benchmark results have been recorded in this document yet. The figures in `docs/performance.md` remain an Tauri baseline and should not be presented as Tauri measurements.

## Migration decisions and remaining work

The current native implementation uses Rust for template expansion, recipient parsing, queue management, config access and Office packaging. The earlier C++/CMake proposal is superseded: it would introduce a second native toolchain and FFI boundary for work already implemented in Rust, with no Tauri benchmark yet showing a CPU bottleneck that needs C++.

Tauri source and dependencies remain in the repository as a fallback until a live Gmail send and a clean-machine install/upgrade confirm Tauri parity. That release validation has not been recorded yet, so the migration is not marked complete.
