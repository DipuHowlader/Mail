# Novamail Tauri migration status

Status: native feature implementation is in place; release parity still needs a live Gmail send and a clean-machine install/upgrade run. Tauri remains available until those gates are complete.

## Native implementation

The React UI keeps its existing `window.api` interface. `src/services/tauriBridge.js` maps it to Tauri commands/events, and the native implementation lives in `src-tauri/src/`.

| Area | Tauri implementation | Status |
| --- | --- | --- |
| Launcher/app roles | One executable starts the launcher or an isolated app process from role arguments. | Implemented |
| Window and taskbar identity | Native window commands; separate WebView2 data directories and AppUserModelID per app instance. | Implemented |
| Launcher control | Authenticated loopback endpoint, heartbeats, reconnect after launcher restart, adoption, graceful close and process-tree fallback. | Implemented |
| Config store | Shared JSON shape; cross-process file lock; reload-before-update; flushed temp file and atomic replace. | Implemented |
| License | Service verification, matching machine id, DPAPI storage under `licenseNative`, and hyphen-preserving key submission. | Implemented; the first Tauri activation is required because Tauri's encrypted blob is incompatible. |
| Campaigns and files | Campaign CRUD, native recipient/template/subject/attachment pickers and CSV/XLSX parsing. | Implemented |
| Template engine | Rust port used by both document conversion and generated mail content. | Implemented |
| Browser driver | `chromiumoxide` drives the bundled Clearcote Chromium through CDP. | Implemented |
| Automation | Queue, retries, pause/resume/stop, telemetry, Gmail login detection, native compose/send, filename-pattern rendering and the 100 KB generated-file ceiling. | Implemented in source; live delivery still needs release validation. |
| Documents | HTML rendering, PDF/text/inline output, image-backed DOCX/XLSX/PPTX, page-sized JPEG capture, adaptive quality/scale, and output-size checks. | Implemented in source; Office opening and size parity need release validation. |
| Packaging | NSIS target; bundles Clearcote runtime and configures WebView2 bootstrapper install. | Configured; clean-machine install/upgrade not yet recorded. |
| Performance coverage | Tauri launcher/isolation/crash scripts and native browser/document benchmark commands. | Isolation and crash tests verified; benchmark results not yet recorded. |

## Decisions

### Rust instead of the proposed C++ core

The earlier proposal placed templates, parsing, queues, license logic and Office packaging in a C++/CMake library. Those responsibilities now live in Rust and the document/browser work is measured through the Tauri benchmark commands. Adding C++ and an FFI boundary would add a second native toolchain without a measured Tauri CPU bottleneck, so C++ is no longer a migration requirement.

### Browser and render path

The React UI runs in Tauri's WebView2 window. Gmail automation and HTML document rendering use the bundled Clearcote Chromium through the Rust `BrowserDriver` trait. This retains the browser behavior used by the existing workflow while removing Tauri from the native execution path.

### App isolation and state

The launcher starts one operating-system process per app. Each process has its own WebView2 folder, automation workers and Chromium sessions. App processes can reconnect to a restarted launcher through the persisted loopback endpoint. The launcher and app processes share only the locked config store and durable instance registry.

## Release gates before deleting Tauri

1. Build the Tauri NSIS installer on the release machine and install it on a clean Windows machine without a preinstalled WebView2 runtime.
2. Activate a valid license key, restart the app, and confirm the stored key re-verifies.
3. Use a real Gmail test account to send a plain-text message, a rich-HTML message and a message with attachments. Confirm successful delivery in Sent and verify an ambiguous send pauses for manual review.
4. Open generated DOCX, XLSX and PPTX files in Office-compatible applications. Measure the preferred and maximum output size behavior with large templates.
5. Run the Tauri launcher/isolation/crash scripts and both Tauri benchmark commands. Record memory, startup, per-email time and process cleanup; compare against the historical Tauri values in `docs/performance.md`.
6. Configure a signing identity and repeat install/upgrade validation for the signed release.
7. After these gates, remove Tauri entry points, dependencies and migration-only scripts, then update the remaining build commands to Tauri.

These are release validation tasks, not source implementation stubs. The Tauri benchmark commands are `npm run benchmark:document:tauri` and `npm run benchmark:browser:tauri`; the launcher scripts end in `:tauri` in `package.json`.
