//! HTML → document export, behind the `document:convert` channel.
//!
//! Ports the *rendering* half of `desktop-runtime/documentEngine.cjs`. The desktop runtime
//! build drove a hidden Chromium to turn expanded HTML into a PDF and then
//! rasterized that PDF into Office formats; here the browser work goes through
//! [`crate::browser::BrowserService`], which drives the same vendored Chromium
//! over the Chrome DevTools Protocol.
//!
//! Supported formats use the same rendered HTML source. Office formats are
//! packaged as valid Office documents whose content is the rendered page
//! rasterized into a JPEG — the exact styled document the preview shows; the
//! browser screenshot is returned for the UI preview.
//!
//! Template expansion is native: `desktop-runtime/documentEngine.cjs` expanded the
//! markup with `parseTemplate` in the main process before rendering, and that
//! engine is now [`crate::template`], so a payload of raw markup + `variables`
//! renders exactly as desktop runtime did.

use std::path::PathBuf;
use std::sync::OnceLock;

use base64::Engine as _;
use regex::{Captures, Regex};
use serde_json::{Map, Value, json};
use tauri::Manager;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::browser::{BrowserService, PdfOptions, paper_size_inches};

/// Opt-in timing for the conversion pipeline, enabled with
/// `NOVAMAIL_CONVERT_TRACE=1`. Prints one `[convert-trace] <label>: <ms>ms`
/// line per phase to stderr so the slow stage is obvious without a profiler.
/// It has no effect on normal runs.
fn trace_enabled() -> bool {
    std::env::var_os("NOVAMAIL_CONVERT_TRACE").is_some()
}

fn trace(label: &str, started: std::time::Instant) {
    if trace_enabled() {
        eprintln!("[convert-trace] {label}: {}ms", started.elapsed().as_millis());
    }
}

fn trace_msg(message: &str) {
    if trace_enabled() {
        eprintln!("[convert-trace] {message}");
    }
}

/// Mirrors `FORMATS` in `desktop-runtime/documentEngine.cjs`.
const FORMATS: [&str; 7] = [
    "inline",
    "inline-text",
    "text",
    "pdf",
    "docx",
    "xlsx",
    "pptx",
];
/// Mirrors `MAX_TEMPLATE_BYTES`.
const MAX_TEMPLATE_BYTES: usize = 2_000_000;
/// Mirrors `MAX_RENDERED_PDF_BYTES`.
const MAX_RENDERED_PDF_BYTES: usize = 50 * 1024 * 1024;
/// Mirrors the desktop runtime attachment budget.
const PREFERRED_ATTACHMENT_BYTES: usize = 100 * 1024;
const MAX_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;

#[tauri::command]
pub async fn document_convert(app: tauri::AppHandle, payload: Value) -> Value {
    let browser = app.state::<BrowserService>();
    match convert(&app, &payload, browser.inner()).await {
        Ok(mut result) => {
            result.insert("ok".to_string(), Value::Bool(true));
            Value::Object(result)
        }

        Err(error) => json!({ "ok": false, "error": error }),
    }
}

pub(crate) async fn convert(
    app: &tauri::AppHandle,
    payload: &Value,
    browser: &BrowserService,
) -> Result<Map<String, Value>, String> {
    let html = payload.get("html").and_then(Value::as_str).unwrap_or("");
    if html.trim().is_empty() {
        return Err("A non-empty \"html\" template is required.".to_string());
    }

    let requested_format = payload.get("format").and_then(Value::as_str).unwrap_or("");
    // Older campaigns use `html` (and the composer uses `inline-html`) for the
    // inline HTML document output. Normalize those aliases before validation so
    // saved drafts and direct callers share the same supported format.
    let format = match requested_format {
        "html" | "inline-html" => "inline",
        format => format,
    };
    if !FORMATS.contains(&format) {
        // Wording (including the missing trailing period) matches desktop runtime's.
        return Err(format!(
            "Unsupported output format \"{format}\". Expected one of: {}",
            FORMATS.join(", ")
        ));
    }

    if html.len() > MAX_TEMPLATE_BYTES {
        return Err(format!(
            "Template exceeds the {}MB size limit.",
            MAX_TEMPLATE_BYTES / 1_000_000
        ));
    }

    // desktop runtime expanded the template in the main process before rendering; the
    // engine now lives in Rust (`template::parse_template`). Unlike desktop runtime we
    // expand *before* the format dispatch, and every downstream step — the PDF,
    // the text extraction and the returned `html` for `inline` — works on the
    // rendered markup, matching `documentEngine.cjs`.
    let convert_started = std::time::Instant::now();
    let variables = payload.get("variables").cloned().unwrap_or(Value::Null);
    let rendered_html = crate::template::parse_template(html, &variables)
        .map_err(|error| format!("Failed to render template: {error}"))?;
    trace("template", convert_started);
    trace_msg(&format!("format={format}"));

    let (extension, mime_type) = match format {
        "inline" | "inline-text" => ("html", "text/html"),
        "text" => ("txt", "text/plain"),
        "pdf" => ("pdf", "application/pdf"),
        "docx" => (
            "docx",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        ),
        "xlsx" => (
            "xlsx",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        ),
        _ => (
            "pptx",
            "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        ),
    };

    // `buffer` is the finished document; `original_size` / `optimized` describe
    // the compaction step, matching desktop runtime's `{ optimized, originalSize }`
    // fields.
    let mut preview_image = None;
    let include_preview = payload
        .get("includePreview")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let (buffer, original_size, optimized): (Vec<u8>, usize, bool) = match format {
        "inline" | "inline-text" => {
            optimize_generated_buffer(rendered_html.as_bytes().to_vec(), format)
        }
        "text" => optimize_generated_buffer(text_from_html(&rendered_html).into_bytes(), "text"),
        "pdf" => {
            let options = pdf_options_from(payload);
            let driver_started = std::time::Instant::now();
            let driver = browser.driver().await?;
            trace("pdf.driver", driver_started);
            let page_started = std::time::Instant::now();
            let page = driver.acquire_page(None).await?;
            trace("pdf.acquire_page", page_started);
            let printed = async {
                let content_started = std::time::Instant::now();
                driver.set_content(page, &rendered_html).await?;
                trace("pdf.set_content", content_started);
                let print_started = std::time::Instant::now();
                let pdf = driver.print_pdf(page, &options).await?;
                trace("pdf.print", print_started);
                let screenshot = if include_preview {
                    let shot_started = std::time::Instant::now();
                    let shot = driver.screenshot_full_page(page).await?;
                    trace("pdf.screenshot", shot_started);
                    shot
                } else {
                    Vec::new()
                };
                Ok::<_, String>((pdf, screenshot))
            }
            .await;
            // Always release the page, even when rendering failed.
            let close_started = std::time::Instant::now();
            let _ = driver.release_page(page).await;
            trace("pdf.release_page", close_started);
            let (pdf, screenshot) = printed?;
            if pdf.len() > MAX_RENDERED_PDF_BYTES {
                return Err(format!(
                    "Rendered PDF exceeds the {} MB processing limit.",
                    MAX_RENDERED_PDF_BYTES / (1024 * 1024)
                ));
            }
            let size = pdf.len();
            if include_preview {
                preview_image = Some(screenshot);
            }
            (pdf, size, false)
        }
        "docx" | "xlsx" | "pptx" => {
            let driver_started = std::time::Instant::now();
            let driver = browser.driver().await?;
            trace("office.driver", driver_started);
            let page_started = std::time::Instant::now();
            let page = driver.acquire_page(None).await?;
            trace("office.acquire_page", page_started);
            // The PDF reflows at the paper's printable width; the capture
            // must do the same or a centered template keeps wide empty sides
            // at the browser window's width. Emulate the PDF content box in
            // CSS pixels (1 CSS px == 9525 EMU of that box).
            let layout = office_frame_layout(&format, &pdf_options_from(payload));
            let (_, _, box_cx, _) = layout.content_box_emu();
            let content_width_px = (box_cx as f64 / 9525.0).round().clamp(120.0, 8_000.0);
            if driver
                .emulate_viewport(page, content_width_px, 1_200.0)
                .await
                .is_err()
            {
                log::warn!("[documents] viewport emulation failed; capturing at the window width");
            }
            // The sent-email container also renders fluid, so the template's
            // own fixed widths must not re-center content inside the emulated
            // page either.
            let document_html = crate::template::responsive_container_html(&rendered_html);
            let rendered = async {
                let content_started = std::time::Instant::now();
                driver.set_content(page, &document_html).await?;
                trace("office.set_content", content_started);
                let screenshot = if include_preview {
                    let shot_started = std::time::Instant::now();
                    let shot = driver.screenshot_full_page(page).await?;
                    trace("office.preview_screenshot", shot_started);
                    shot
                } else {
                    Vec::new()
                };
                let dimensions = driver.evaluate(page, PAGE_DIMENSIONS_JS).await?;
                let (pixel_width, pixel_height) = page_dimensions(&dimensions)?;
                let (office, original_size, optimized) = adaptive_office_buffer(
                    &driver,
                    page,
                    format,
                    &layout,
                    pixel_width,
                    pixel_height,
                )
                .await?;
                Ok::<_, String>((screenshot, office, original_size, optimized))
            }
            .await;
            let close_started = std::time::Instant::now();
            // Restore the pooled page to its real window viewport.
            let _ = driver.clear_viewport_emulation(page).await;
            let _ = driver.release_page(page).await;
            trace("office.release_page", close_started);
            let (screenshot, office, original_size, optimized) = rendered?;
            if include_preview {
                preview_image = Some(screenshot);
            }
            (office, original_size, optimized)
        }
        other => {
            return Err(format!(
                "{other} export is not available in this build yet."
            ));
        }
    };

    let filename = generate_filename(
        payload
            .get("baseName")
            .and_then(Value::as_str)
            .unwrap_or(""),
        extension,
    );

    let mut result = Map::new();
    result.insert("format".to_string(), json!(format));
    result.insert("extension".to_string(), json!(extension));
    result.insert("mimeType".to_string(), json!(mime_type));
    result.insert("filename".to_string(), json!(filename));
    let encode_started = std::time::Instant::now();
    result.insert(
        "buffer".to_string(),
        json!(base64::engine::general_purpose::STANDARD.encode(&buffer)),
    );
    trace("base64.buffer", encode_started);
    result.insert("originalSize".to_string(), json!(original_size));
    result.insert("optimized".to_string(), json!(optimized));
    result.insert("fileSize".to_string(), json!(buffer.len()));
    // desktop runtime also reports the attachment budget and, for inline output, the
    // un-compacted markup alongside the compacted `buffer`.
    result.insert(
        "preferredSize".to_string(),
        json!(PREFERRED_ATTACHMENT_BYTES),
    );
    result.insert("maximumSize".to_string(), json!(MAX_ATTACHMENT_BYTES));
    if matches!(format, "inline" | "inline-text") {
        result.insert("html".to_string(), json!(rendered_html));
    }
    if let Some(image) = preview_image {
        result.insert(
            "previewImage".to_string(),
            json!(base64::engine::general_purpose::STANDARD.encode(image)),
        );
        result.insert("previewMimeType".to_string(), json!("image/png"));
    }

    if payload
        .get("saveToDisk")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let dir = payload
            .get("saveDir")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| default_export_dir(app));
        std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
        let path = dir.join(
            result
                .get("filename")
                .and_then(Value::as_str)
                .unwrap_or("document"),
        );
        write_file_atomic(&path, &buffer)?;
        result.insert("filePath".to_string(), json!(path.display().to_string()));
    }

    trace("total", convert_started);
    Ok(result)
}

/// Default export folder, mirroring desktop runtime's `Documents/NovaBlast/Exports`.
fn default_export_dir(app: &tauri::AppHandle) -> PathBuf {
    app.path()
        .document_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("NovaBlast")
        .join("Exports")
}

fn write_file_atomic(path: &std::path::Path, contents: &[u8]) -> Result<(), String> {
    let temp = path.with_file_name(format!(
        ".{}-{}.tmp",
        path.file_name()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or("document"),
        uuid::Uuid::new_v4().simple()
    ));
    let result = (|| {
        use std::io::Write;

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|error| error.to_string())?;
        file.write_all(contents)
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        std::fs::rename(&temp, path).map_err(|error| error.to_string())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result
}

#[cfg(windows)]
fn current_working_set_mb() -> Option<u64> {
    use std::ffi::c_void;

    #[repr(C)]
    #[derive(Default)]
    #[allow(dead_code)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
    }
    #[link(name = "psapi")]
    unsafe extern "system" {
        fn GetProcessMemoryInfo(
            process: *mut c_void,
            counters: *mut ProcessMemoryCounters,
            size: u32,
        ) -> i32;
    }

    let mut counters = ProcessMemoryCounters {
        cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
        ..Default::default()
    };
    let size = counters.cb;
    // SAFETY: the process handle is the current process and the counters value
    // has the C layout and size required by GetProcessMemoryInfo.
    let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, size) };
    (ok != 0).then_some((counters.working_set_size as u64) / (1024 * 1024))
}

#[cfg(not(windows))]
fn current_working_set_mb() -> Option<u64> {
    None
}

/// Read the (optional) `pdfOptions` the renderer may pass.
fn pdf_options_from(payload: &Value) -> PdfOptions {
    let mut options = PdfOptions::default();
    let Some(pdf) = payload.get("pdfOptions").and_then(Value::as_object) else {
        return options;
    };
    if let Some(size) = pdf.get("pageSize").and_then(Value::as_str) {
        if !size.trim().is_empty() {
            options.page_size = size.to_string();
        }
    }
    if let Some(landscape) = pdf.get("landscape").and_then(Value::as_bool) {
        options.landscape = landscape;
    }
    options
}

/// Office page geometry, mirrored from the PDF path: the same named paper
/// size (`browser::paper_size_inches`), the same margins and the same
/// landscape orientation. Every office document therefore shows the content
/// exactly where the PDF places it — same page shape, same margins, same
/// usable area — instead of letter-sized pages with one-inch borders.
struct PaperLayout {
    /// Page dimensions in millimetres (orientation already applied).
    page_w_mm: f64,
    page_h_mm: f64,
    margin_top_mm: f64,
    margin_bottom_mm: f64,
    margin_left_mm: f64,
    margin_right_mm: f64,
}

impl PaperLayout {
    /// Page extents in Word twips (1/1440 inch). A4 rounds to Word's
    /// canonical 11906×16838; the largest supported paper stays under the
    /// 22-inch Word limit.
    fn page_twips(&self) -> (u64, u64) {
        const TWIPS_PER_MM: f64 = 1440.0 / 25.4;
        (
            (self.page_w_mm * TWIPS_PER_MM).round().max(1.0) as u64,
            (self.page_h_mm * TWIPS_PER_MM).round().max(1.0) as u64,
        )
    }

    /// Box margins in Word twips.
    fn margin_twips(&self) -> (u64, u64, u64, u64) {
        const TWIPS_PER_MM: f64 = 1440.0 / 25.4;
        (
            (self.margin_top_mm * TWIPS_PER_MM).round() as u64,
            (self.margin_right_mm * TWIPS_PER_MM).round() as u64,
            (self.margin_bottom_mm * TWIPS_PER_MM).round() as u64,
            (self.margin_left_mm * TWIPS_PER_MM).round() as u64,
        )
    }

    /// The usable content box in EMU — exactly the box the PDF prints into:
    /// `a:off` position and `wp:extent` bounds for the rendered page image.
    fn content_box_emu(&self) -> (i64, i64, u64, u64) {
        const EMU_PER_MM: f64 = 36_000.0;
        let x = self.margin_left_mm * EMU_PER_MM;
        let y = self.margin_top_mm * EMU_PER_MM;
        let cx = ((self.page_w_mm - self.margin_left_mm - self.margin_right_mm)
            * EMU_PER_MM)
            .max(1.0);
        let cy = ((self.page_h_mm - self.margin_top_mm - self.margin_bottom_mm)
            * EMU_PER_MM)
            .max(1.0);
        (x.round() as i64, y.round() as i64, cx.round() as u64, cy.round() as u64)
    }

    /// The full page in EMU, for the PowerPoint slide size.
    fn page_emu(&self) -> (u64, u64) {
        const EMU_PER_MM: f64 = 36_000.0;
        (
            (self.page_w_mm * EMU_PER_MM).round().max(1.0) as u64,
            (self.page_h_mm * EMU_PER_MM).round().max(1.0) as u64,
        )
    }
}

/// Derives the office page geometry from the very options the PDF printer
/// consumes, so the paper shape and borders agree across all four formats.
fn paper_layout(options: &PdfOptions) -> PaperLayout {
    // Same mapping (and fallback) as the PDF print: an unknown paper name
    // falls back to A4 portrait there too.
    let (w_in, h_in) =
        paper_size_inches(&options.page_size).unwrap_or((8.27, 11.69));
    let (w_in, h_in) = if options.landscape { (h_in, w_in) } else { (w_in, h_in) };
    PaperLayout {
        page_w_mm: w_in * 25.4,
        page_h_mm: h_in * 25.4,
        margin_top_mm: options.margins.top_mm,
        margin_bottom_mm: options.margins.bottom_mm,
        margin_left_mm: options.margins.left_mm,
        margin_right_mm: options.margins.right_mm,
    }
}

/// Thinner borders for the DOCX and XLSX page: those documents are read on
/// screen where no printer clips them, so the same A4 paper can carry a
/// larger content box — and with it slightly larger, easier-to-read text.
/// PPTX and the PDF keep the exact PDF margins for a like-for-like match.
const OFFICE_MARGIN_TOP_MM: f64 = 6.0;
const OFFICE_MARGIN_BOTTOM_MM: f64 = 6.0;
const OFFICE_MARGIN_LEFT_MM: f64 = 5.0;
const OFFICE_MARGIN_RIGHT_MM: f64 = 5.0;

fn slim_office_margins(mut layout: PaperLayout) -> PaperLayout {
    layout.margin_top_mm = OFFICE_MARGIN_TOP_MM;
    layout.margin_bottom_mm = OFFICE_MARGIN_BOTTOM_MM;
    layout.margin_left_mm = OFFICE_MARGIN_LEFT_MM;
    layout.margin_right_mm = OFFICE_MARGIN_RIGHT_MM;
    layout
}

/// The office frame for one export: the PDF paper and — for DOCX/XLSX — the
/// slimmer on-screen margins; PPTX keeps the PDF margins exact.
fn office_frame_layout(format: &str, options: &PdfOptions) -> PaperLayout {
    let layout = paper_layout(options);
    if format == "pptx" {
        layout
    } else {
        slim_office_margins(layout)
    }
}

/// The document/compaction regexes are compiled once and reused. Recompiling
/// them on every call was measurably expensive (`text_from_html` alone compiled
/// six patterns per export).
fn regex_comment() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)<!--.*?-->").expect("comment regex is valid"))
}

fn regex_between_tags() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r">\s+<").expect("between-tags regex is valid"))
}

fn regex_spaces() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[ \t]+").expect("spaces regex is valid"))
}

fn regex_blank_lines() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\n{3,}").expect("blank-lines regex is valid"))
}

fn regex_style_script_blocks() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?is)<style[\s\S]*?</style>|<script[\s\S]*?</script>")
            .expect("style/script block regex is valid")
    })
}

fn regex_br() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)<br\s*/?>").expect("br regex is valid"))
}

fn regex_block_close() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)</(?:p|div|tr|h[1-6]|li)>").expect("block-close regex is valid"))
}

fn regex_tags() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"<[^>]+>").expect("tags regex is valid"))
}

fn regex_entities() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)&(nbsp|amp|lt|gt|quot|#39);").expect("entities regex is valid")
    })
}

fn office_buffer(
    format: &str,
    image: &[u8],
    layout: &PaperLayout,
    pixel_width: u32,
    pixel_height: u32,
) -> Result<Vec<u8>, String> {
    if image.len() < 4 || image[..2] != [0xff, 0xd8] {
        return Err("The rendered office document did not produce a valid JPEG image.".to_string());
    }
    // Every office format package carries exactly one piece of content: the
    // rendered page as a JPEG laid out exactly like the PDF page — same paper
    // size, same margins, same content box — so all four formats agree in
    // size, spacing and readability. The extracted HTML source text never
    // goes into office files.
    let mut entries: Vec<(&str, String)> = Vec::new();
    let root_rels = r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="TARGET"/></Relationships>"#;
    match format {
        "docx" => {
            // Page and margins mirror the PDF; the picture is scaled to the
            // full printable width and page height.
            let (page_w, page_h) = layout.page_twips();
            let (margin_top, margin_right, margin_bottom, margin_left) = layout.margin_twips();
            let (_, _, box_cx, box_cy) = layout.content_box_emu();
            let (cx, cy) = fit_image_extent(pixel_width, pixel_height, box_cx, box_cy);
            entries.push(("[Content_Types].xml", r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="jpg" ContentType="image/jpeg"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#.to_string()));
            entries.push((
                "_rels/.rels",
                root_rels.replace("TARGET", "word/document.xml"),
            ));
            entries.push(("word/_rels/document.xml.rels", r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/image1.jpg"/></Relationships>"#.to_string()));
            entries.push(("word/document.xml", format!(r#"<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture"><w:body><w:p><w:r><w:drawing><wp:inline distT="0" distB="0" distL="0" distR="0"><wp:extent cx="{cx}" cy="{cy}"/><wp:docPr id="1" name="Rendered document"/><wp:cNvGraphicFramePr/><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/picture"><pic:pic><pic:nvPicPr><pic:cNvPr id="0" name="Rendered document"/><pic:cNvPicPr/></pic:nvPicPr><pic:blipFill><a:blip r:embed="rId1"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill><pic:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="{cx}" cy="{cy}"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></pic:spPr></pic:pic></a:graphicData></a:graphic></wp:inline></w:drawing></w:r></w:p><w:sectPr><w:pgSz w:w="{page_w}" w:h="{page_h}"/><w:pgMar w:top="{margin_top}" w:right="{margin_right}" w:bottom="{margin_bottom}" w:left="{margin_left}" w:header="708" w:footer="708" w:gutter="0"/></w:sectPr></w:body></w:document>"#)));
        }
        "xlsx" => {
            // A one-cell anchor with the PDF content-box extent — the sheet
            // shows the document at the same scale the PDF pages do, and its
            // size no longer depends on text rows. No extracted text dump.
            let (_, _, box_cx, box_cy) = layout.content_box_emu();
            let (cx, cy) = fit_image_extent(pixel_width, pixel_height, box_cx, box_cy);
            entries.push(("[Content_Types].xml", r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="jpg" ContentType="image/jpeg"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/drawings/drawing1.xml" ContentType="application/vnd.openxmlformats-officedocument.drawing+xml"/></Types>"#.to_string()));
            entries.push((
                "_rels/.rels",
                root_rels.replace("TARGET", "xl/workbook.xml"),
            ));
            entries.push(("xl/workbook.xml", r#"<?xml version="1.0" encoding="UTF-8"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Document" sheetId="1" r:id="rId1"/></sheets></workbook>"#.into()));
            entries.push(("xl/_rels/workbook.xml.rels", r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#.into()));
            entries.push(("xl/worksheets/_rels/sheet1.xml.rels", r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#.to_string()));
            entries.push(("xl/worksheets/sheet1.xml", r#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheetData/><drawing r:id="rId1"/></worksheet>"#.to_string()));
            entries.push(("xl/drawings/_rels/drawing1.xml.rels", r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/image1.jpg"/></Relationships>"#.to_string()));
            entries.push(("xl/drawings/drawing1.xml", format!(r#"<?xml version="1.0" encoding="UTF-8"?><xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><xdr:oneCellAnchor><xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from><xdr:ext cx="{cx}" cy="{cy}"/><xdr:pic><xdr:nvPicPr><xdr:cNvPr id="2" name="Rendered document"/><xdr:cNvPicPr><a:picLocks noChangeAspect="1"/></xdr:cNvPicPr></xdr:nvPicPr><xdr:blipFill><a:blip r:embed="rId1"/><a:stretch><a:fillRect/></a:stretch></xdr:blipFill><xdr:spPr><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></xdr:spPr></xdr:pic><xdr:clientData/></xdr:oneCellAnchor></xdr:wsDr>"#)));
        }
        "pptx" => {
            // The slide itself is the PDF page: same paper size, and the
            // picture sits in the same margins box the PDF content occupies.
            // The package mirrors the python-pptx default skeleton (theme,
            // presentation properties, view properties, table styles, a
            // master with a background, and a blank layout) — the shape
            // PowerPoint provably opens without a repair pass.
            let (slide_cx, slide_cy) = layout.page_emu();
            let (off_x, off_y, box_cx, box_cy) = layout.content_box_emu();
            let (cx, cy) = fit_image_extent(pixel_width, pixel_height, box_cx, box_cy);
            entries.push(("[Content_Types].xml", r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="jpg" ContentType="image/jpeg"/><Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/><Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/><Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/><Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/><Override PartName="/ppt/presProps.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presProps+xml"/><Override PartName="/ppt/viewProps.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.viewProps+xml"/><Override PartName="/ppt/tableStyles.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.tableStyles+xml"/><Override PartName="/ppt/theme/theme1.xml" ContentType="application/vnd.openxmlformats-officedocument.theme+xml"/></Types>"#.to_string()));
            entries.push((
                "_rels/.rels",
                root_rels.replace("TARGET", "ppt/presentation.xml"),
            ));
            entries.push(("ppt/presentation.xml", format!(r#"<?xml version="1.0" encoding="UTF-8"?><p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rId1"/></p:sldMasterIdLst><p:sldIdLst><p:sldId id="256" r:id="rId2"/></p:sldIdLst><p:sldSz cx="{slide_cx}" cy="{slide_cy}"/><p:notesSz cx="6858000" cy="9144000"/></p:presentation>"#)));
            entries.push(("ppt/_rels/presentation.xml.rels", r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="slideMasters/slideMaster1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/presProps" Target="presProps.xml"/><Relationship Id="rId4" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/viewProps" Target="viewProps.xml"/><Relationship Id="rId5" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="theme/theme1.xml"/><Relationship Id="rId6" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/tableStyles" Target="tableStyles.xml"/></Relationships>"#.to_string()));
            entries.push(("ppt/presProps.xml", crate::office_parts::PPTX_PRES_PROPS.to_string()));
            entries.push(("ppt/viewProps.xml", crate::office_parts::PPTX_VIEW_PROPS.to_string()));
            entries.push(("ppt/tableStyles.xml", crate::office_parts::PPTX_TABLE_STYLES.to_string()));
            entries.push(("ppt/theme/theme1.xml", crate::office_parts::PPTX_THEME.to_string()));
            // The master needs a background fill: without `p:bg` PowerPoint
            // falls back to theme look-ups and shows the slide blank.
            entries.push(("ppt/slideMasters/slideMaster1.xml", r#"<?xml version="1.0" encoding="UTF-8"?><p:sldMaster xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><p:cSld name="Novamail"><p:bg><p:bgRef idx="1001"><a:schemeClr val="bg1"/></p:bgRef></p:bg><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr></p:spTree></p:cSld><p:clrMap accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" bg1="lt1" bg2="lt2" dk1="dk1" dk2="dk2" folHlink="folHlink" hlink="hlink" tx1="dk1" tx2="dk2"/><p:sldLayoutIdLst><p:sldLayoutId id="1" r:id="rId1"/></p:sldLayoutIdLst></p:sldMaster>"#.to_string()));
            entries.push(("ppt/slideMasters/_rels/slideMaster1.xml.rels", r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="../theme/theme1.xml"/></Relationships>"#.to_string()));
            entries.push(("ppt/slideLayouts/slideLayout1.xml", r#"<?xml version="1.0" encoding="UTF-8"?><p:sldLayout xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" type="blank" preserve="1"><p:cSld name="Blank"><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr></p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sldLayout>"#.to_string()));
            entries.push(("ppt/slideLayouts/_rels/slideLayout1.xml.rels", r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="../slideMasters/slideMaster1.xml"/></Relationships>"#.to_string()));
            entries.push(("ppt/slides/_rels/slide1.xml.rels", r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/image1.jpg"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/></Relationships>"#.to_string()));
            entries.push(("ppt/slides/slide1.xml", format!(r#"<?xml version="1.0" encoding="UTF-8"?><p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr><p:pic><p:nvPicPr><p:cNvPr id="2" name="Rendered document"/><p:cNvPicPr><a:picLocks noChangeAspect="1"/></p:cNvPicPr><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="rId1"/><a:stretch><a:fillRect/></a:stretch></p:blipFill><p:spPr><a:xfrm><a:off x="{off_x}" y="{off_y}"/><a:ext cx="{cx}" cy="{cy}"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr></p:pic></p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>"#)));
        }
        _ => unreachable!(),
    }
    let mut output = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for (name, content) in entries {
        output
            .start_file(name, options)
            .map_err(|error| error.to_string())?;
        std::io::Write::write_all(&mut output, content.as_bytes())
            .map_err(|error| error.to_string())?;
    }
    let media_path = match format {
        "docx" => "word/media/image1.jpg",
        "xlsx" => "xl/media/image1.jpg",
        "pptx" => "ppt/media/image1.jpg",
        _ => unreachable!(),
    };
    // The image is already entropy-coded JPEG, so deflating it costs real CPU
    // (the dominant cost of a large export) for essentially no size gain. Store
    // it verbatim; the tiny XML parts above stay deflated.
    let media_options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    output
        .start_file(media_path, media_options)
        .map_err(|error| error.to_string())?;
    std::io::Write::write_all(&mut output, image).map_err(|error| error.to_string())?;
    output
        .finish()
        .map(|cursor| cursor.into_inner())
        .map_err(|error| error.to_string())
}

/// JS expression that reports the rendered page's full pixel dimensions. Used
/// by the CDP screenshot fallback and the full-page capture.
const PAGE_DIMENSIONS_JS: &str = "({width: Math.ceil(Math.max(document.documentElement.scrollWidth, document.body?.scrollWidth || 0, innerWidth)), height: Math.ceil(Math.max(document.documentElement.scrollHeight, document.body?.scrollHeight || 0, innerHeight))})";

/// The office attachment budget mirrors the generated-attachment preference
/// (100 KB). Reaching it must not blur the text though: the ladder starts
/// from a ~300 dpi capture and backs the resolution off in small steps while
/// keeping JPEG quality >= 65, so the accepted capture still prints at
/// ~200+ dpi for ordinary pages — versus ~40-80 dpi under the old
/// quality-40/0.3-scale ladder. Gmail's 25 MB ceiling stays the hard stop.
const OFFICE_PREFERRED_BYTES: usize = 100 * 1024;

/// The JPEG quality/scale ladder, tried in order. The first entry is the
/// ideal high-DPI, high-quality capture; every later entry multiplies the
/// capture scale by a factor < 1 only when the package is still too large.
/// Quality leads the descent: JPEG ringing on text turns ugly below
/// ~quality 65, while a modestly smaller dpi still reads sharp — so quality
/// floors at 65 and the scale does the rest of the compression. The steps
/// near the 100 KB boundary are deliberately fine (~10% scale), so the
/// accepted capture sits just under the budget at the highest dpi/quality
/// the size allows instead of comfortably below it.
const OFFICE_LADDER: [(f64, u8); 10] = [
    (1.0, 92),
    (1.0, 80),
    (1.0, 66),
    (0.9, 85),
    (0.8, 85),
    (0.7, 85),
    (0.6, 82),
    (0.5, 78),
    (0.4, 70),
    (0.3, 65),
];

/// The capture zoom that renders the page at roughly 300 dpi once the image
/// is fitted into the PDF content box (the returned EMU extent prints at
/// `cy / 914400` inches). CSS pixel captures are only ~130 dpi there, which
/// reads as blurry next to the vector PDF; the browser re-rasterizes text and
/// shapes at the zoom level, so the result is sharp, not upscaled.
fn office_capture_scale(layout: &PaperLayout, pixel_height: u32) -> f64 {
    let (_, _, _, box_cy) = layout.content_box_emu();
    let content_height_in = box_cy as f64 / 914400.0;
    (content_height_in * 300.0 / f64::from(pixel_height.max(1)))
        .clamp(1.0, 3.0)
        // Keep the output height (and so the capture surface) bounded even for
        // extremely tall documents; they already exceed 300 dpi at zoom 1.
        .min(32_000.0 / f64::from(pixel_height.max(1)))
        .max(1.0)
}

async fn adaptive_office_buffer(
    driver: &std::sync::Arc<dyn crate::browser::BrowserDriver>,
    page: u64,
    format: &str,
    layout: &PaperLayout,
    pixel_width: u32,
    pixel_height: u32,
) -> Result<(Vec<u8>, usize, bool), String> {
    // Reduce JPEG quality first (floored high), then scale the captured page
    // down, without causing CSS reflow. The zero-degradation capture is a
    // ~300 dpi zoom; the 100 KB attachment preference is honored by scaling
    // the capture in small steps — never by crushing quality below 65 — and
    // the 25 MB limit remains the absolute stop.
    let target_scale = office_capture_scale(layout, pixel_height);
    let mut best: Option<(Vec<u8>, bool)> = None;
    let mut original_size = 0usize;
    let mut last_capture_error = None;
    for (attempt, (factor, quality)) in OFFICE_LADDER.into_iter().enumerate() {
        let scale = target_scale * factor;
        let attempt_started = std::time::Instant::now();
        // Reuse the dimensions measured once above instead of re-evaluating
        // them on every attempt.
        let image = match driver
            .screenshot_jpeg_region(page, quality, scale, pixel_width as f64, pixel_height as f64)
            .await
        {
            Ok(image) => image,
            Err(error) => {
                last_capture_error = Some(error);
                continue;
            }
        };
        let capture_ms = attempt_started.elapsed().as_millis();
        let zip_started = std::time::Instant::now();
        let package = office_buffer(format, &image, layout, pixel_width, pixel_height)?;
        let zip_ms = zip_started.elapsed().as_millis();
        trace_msg(&format!(
            "office.attempt attempt={attempt} scale={scale:.2} quality={quality} capture={capture_ms}ms zip={zip_ms}ms image={}B package={}B",
            image.len(),
            package.len()
        ));
        if original_size == 0 {
            original_size = package.len();
        }
        let package_size = package.len();
        // The first ladder entry is the ideal capture; anything that had to
        // back off counts as degraded.
        let degraded = attempt != 0;
        if best
            .as_ref()
            .map(|(candidate, _)| package_size < candidate.len())
            .unwrap_or(true)
        {
            best = Some((package, degraded));
        }
        if package_size <= OFFICE_PREFERRED_BYTES {
            let (package, degraded) = best.unwrap();
            return Ok((package, original_size, degraded));
        }
    }

    let (package, degraded) = best.ok_or_else(|| {
        last_capture_error
            .unwrap_or_else(|| "Could not optimize the rendered Office attachment.".to_string())
    })?;
    if package.len() > MAX_ATTACHMENT_BYTES {
        return Err(format!(
            "Rendered {} attachment exceeds the 25 MB processing limit after optimization.",
            format.to_uppercase()
        ));
    }
    Ok((package, original_size, degraded))
}

fn page_dimensions(value: &Value) -> Result<(u32, u32), String> {
    let width = value.get("width").and_then(Value::as_u64).unwrap_or(0);
    let height = value.get("height").and_then(Value::as_u64).unwrap_or(0);
    if width == 0 || height == 0 {
        return Err("The rendered office document has invalid dimensions.".to_string());
    }
    Ok((
        u32::try_from(width)
            .map_err(|_| "The rendered office document is too wide.".to_string())?,
        u32::try_from(height)
            .map_err(|_| "The rendered office document is too tall.".to_string())?,
    ))
}

/// Converts the rendered page's CSS pixels to EMU extents for the office
/// packages. One CSS pixel at the renderer's 96 dpi is exactly 9525 EMU;
/// extents larger than `max_width`/`max_height` shrink proportionally so the
/// picture fits the target document page at its real size — not the tiny dot
/// the previous px→px ratio produced.
fn fit_image_extent(width: u32, height: u32, max_width: u64, max_height: u64) -> (u64, u64) {
    const EMU_PER_PIXEL: f64 = 9525.0; // 914400 EMU per inch / 96 dpi
    let natural_width = width as f64 * EMU_PER_PIXEL;
    let natural_height = height as f64 * EMU_PER_PIXEL;
    let scale = (max_width as f64 / natural_width)
        .min(max_height as f64 / natural_height)
        .min(1.0);
    (
        (natural_width * scale).round().max(1.0) as u64,
        (natural_height * scale).round().max(1.0) as u64,
    )
}

/// `<safe-base>-<uuid>.<ext>`, compatible with desktop runtime's `generateFilename()`:
/// every run of characters outside `[A-Za-z0-9._-]` collapses to one dash.
fn generate_filename(base_name: &str, extension: &str) -> String {
    let mut safe = String::new();
    let mut pending_dash = false;
    for ch in base_name.trim().chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            safe.push(ch);
            pending_dash = false;
        } else if !pending_dash {
            safe.push('-');
            pending_dash = true;
        }
    }
    let sliced: String = safe.trim_matches('-').chars().take(60).collect();
    let base = if sliced.is_empty() {
        "document".to_string()
    } else {
        sliced
    };
    format!("{base}-{}.{extension}", uuid::Uuid::new_v4())
}

/// Run the document-render concurrency benchmark against the same command path
/// the composer uses. This is opt-in through `NOVAMAIL_SELFTEST_MODE` so normal
/// app startup never creates benchmark output.
pub async fn run_benchmark(app: tauri::AppHandle) -> Result<Value, String> {
    let browser = app.state::<BrowserService>();
    let levels = std::env::var("NOVAMAIL_BENCHMARK_LEVELS")
        .ok()
        .map(|raw| {
            raw.split(',')
                .filter_map(|part| part.trim().parse::<usize>().ok())
                .filter(|level| (1..=16).contains(level))
                .collect::<Vec<_>>()
        })
        .filter(|levels| !levels.is_empty())
        .unwrap_or_else(|| vec![1, 2, 3, 4, 5]);
    let root = std::env::temp_dir().join(format!(
        "novamail-tauri-document-benchmark-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&root).map_err(|error| error.to_string())?;
    let mut reports = Vec::new();

    let result = async {
        for level in levels {
            let output_dir = root.join(format!("level-{level}"));
            std::fs::create_dir_all(&output_dir).map_err(|error| error.to_string())?;
            let started = std::time::Instant::now();
            let tasks = (0..level).map(|index| {
                let app = app.clone();
                let browser = browser.inner();
                let html = format!(
                    "<html><body><h1>Level {level}, document {index}</h1><p>{}</p><p>{}</p></body></html>",
                    "benchmark output ".repeat(150),
                    uuid::Uuid::new_v4()
                );
                let payload = json!({
                    "html": html,
                    "format": "pdf",
                    "baseName": format!("tauri-{level}-{index}"),
                    "saveToDisk": true,
                    "saveDir": output_dir.display().to_string(),
                });
                async move {
                    let call_started = std::time::Instant::now();
                    let result = convert(&app, &payload, browser).await?;
                    Ok::<_, String>((
                        call_started.elapsed().as_millis() as u64,
                        result.get("filename").and_then(Value::as_str).unwrap_or("").to_string(),
                        result.get("fileSize").and_then(Value::as_u64).unwrap_or(0),
                    ))
                }
            });
            let results = futures::future::join_all(tasks).await;
            let mut durations = Vec::with_capacity(level);
            let mut names = Vec::with_capacity(level);
            let mut total_bytes = 0u64;
            for entry in results {
                let (elapsed_ms, filename, bytes) = entry?;
                durations.push(elapsed_ms);
                names.push(filename);
                total_bytes += bytes;
            }
            let files_on_disk = std::fs::read_dir(&output_dir)
                .map_err(|error| error.to_string())?
                .filter_map(Result::ok)
                .count();
            let max_call_ms = durations.iter().copied().max().unwrap_or(0);
            let mean_call_ms = durations.iter().sum::<u64>() / durations.len().max(1) as u64;
            reports.push(json!({
                "level": level,
                "totalMs": started.elapsed().as_millis(),
                "maxCallMs": max_call_ms,
                "meanCallMs": mean_call_ms,
                "bytes": total_bytes,
                "workingSetMb": current_working_set_mb(),
                "filesOnDisk": files_on_disk,
                "uniqueFilenames": names.iter().collect::<std::collections::HashSet<_>>().len(),
            }));
        }
        Ok::<_, String>(json!({ "engine": "tauri/chromiumoxide", "levels": reports }))
    }
    .await;

    let _ = std::fs::remove_dir_all(&root);
    result
}

/// Compact `inline`/`text` output the way `optimizeGeneratedBuffer` does.
///
/// Returns `(buffer, originalSize, optimized)`; when compaction would not help,
/// the original buffer is returned unchanged with `optimized: false`.
fn optimize_generated_buffer(buffer: Vec<u8>, format: &str) -> (Vec<u8>, usize, bool) {
    let original_size = buffer.len();
    let Ok(source) = std::str::from_utf8(&buffer) else {
        return (buffer, original_size, false);
    };
    let compact = match format {
        "inline" | "inline-text" => {
            let stripped = regex_comment().replace_all(source, "");
            regex_between_tags()
                .replace_all(&stripped, "><")
                .trim()
                .to_string()
        }
        "text" => {
            let collapsed = regex_spaces().replace_all(source, " ");
            regex_blank_lines()
                .replace_all(&collapsed, "\n\n")
                .trim()
                .to_string()
        }
        _ => return (buffer, original_size, false),
    };
    if compact.len() < original_size {
        (compact.into_bytes(), original_size, true)
    } else {
        (buffer, original_size, false)
    }
}

/// Port of `textFromHtml()` from `desktop-runtime/documentEngine.cjs`.
fn text_from_html(html: &str) -> String {
    let stripped = regex_style_script_blocks().replace_all(html, " ");
    let with_breaks = regex_br().replace_all(&stripped, "\n");
    let with_blocks = regex_block_close().replace_all(&with_breaks, "\n");
    let no_tags = regex_tags().replace_all(&with_blocks, " ");
    let decoded = decode_entities(&no_tags);
    let collapsed = regex_spaces().replace_all(&decoded, " ");
    let compact = regex_blank_lines().replace_all(&collapsed, "\n\n");
    compact
        .split('\n')
        .map(str::trim)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// Replace HTML entities the way `textFromHtml()` does (`&nbsp;` → space, etc.).
fn decode_entities(source: &str) -> String {
    let entities = regex_entities();
    entities
        .replace_all(source, |caps: &Captures| {
            let name = caps[1].to_ascii_lowercase();
            match name.as_str() {
                "nbsp" => " ".to_string(),
                "amp" => "&".to_string(),
                "lt" => "<".to_string(),
                "gt" => ">".to_string(),
                "quot" => "\"".to_string(),
                _ => "'".to_string(),
            }
        })
        .into_owned()
}

/// Headless self-test for the document engine, driven by `selftest.rs`.
///
/// Exercises the real command path (`convert`) so format dispatch, compaction,
/// filename generation, base64 encoding and the PDF renderer are all covered
/// without a UI.
pub async fn selftest(app: &tauri::AppHandle) -> Result<Value, String> {
    let state = app.state::<BrowserService>();
    let browser = state.inner();
    let mut report = Map::new();
    let mut passed = true;

    let inline = convert(
        app,
        &json!({ "html": "<p>Hello</p>\n\n<p>World</p>", "format": "inline", "baseName": "Selftest 1" }),
        browser,
    )
    .await?;
    let inline_ok = decode_text(&inline)? == "<p>Hello</p><p>World</p>"
        && filename_is(&inline, "Selftest-1-", "html");
    passed &= inline_ok;
    report.insert("inline".into(), json!(inline_ok));

    let text = convert(
        app,
        &json!({ "html": "<h1>Hello</h1><p>A&nbsp;B</p><br>Second", "format": "text", "baseName": "Selftest 2" }),
        browser,
    )
    .await?;
    let text_value = decode_text(&text)?;
    let text_ok = text_value == "Hello\nA B\n\nSecond" && filename_is(&text, "Selftest-2-", "txt");
    passed &= text_ok;
    report.insert("text".into(), json!(text_ok));
    report.insert("textValue".into(), json!(text_value));

    let pdf = convert(
        app,
        &json!({ "html": "<h1>Hello</h1>", "format": "pdf", "baseName": "Selftest 3" }),
        browser,
    )
    .await?;
    let pdf_bytes = decode_bytes(&pdf)?;
    let pdf_ok = pdf_bytes.starts_with(b"%PDF-")
        && pdf_bytes.len() > 1_000
        && filename_is(&pdf, "Selftest-3-", "pdf");
    passed &= pdf_ok;
    report.insert("pdf".into(), json!(pdf_ok));
    report.insert("pdfBytes".into(), json!(pdf_bytes.len()));

    for format in ["docx", "xlsx", "pptx"] {
        // The document carries a fixed-width container on purpose: the office
        // path must normalize it (`responsive_container_html`) reflow at the
        // paper width and still export — a broken container pass (e.g. an
        // unsupported regex construct) would panic here and skip this check.
        let office = convert(
            app,
            &json!({
                "html": "<div style=\"width:600px;margin:0 auto\"><h1>Hello</h1><table width=\"600\" align=\"center\"><tr><td width=\"24\"></td><td><p>Office export</p></td></tr></table></div>",
                "format": format,
                "baseName": "Selftest Office"
            }),
            browser,
        )
        .await?;
        let office_bytes = decode_bytes(&office)?;
        let office_ok = office_bytes.starts_with(b"PK\x03\x04")
            && office
                .get("previewImage")
                .and_then(Value::as_str)
                .map(|value| !value.is_empty())
                .unwrap_or(false)
            && filename_is(&office, "Selftest-Office-", format);
        passed &= office_ok;
        report.insert(format.to_string(), json!(office_ok));
    }

    // Template expansion must match desktop runtime's `parseTemplate` before rendering.
    let templated = convert(
        app,
        &json!({
            "html": "<p>{{name}} owes {{amount | currency:'USD'}}</p>",
            "format": "inline",
            "baseName": "Selftest 5",
            "variables": { "name": "Ada & Sons", "amount": 1234.5 }
        }),
        browser,
    )
    .await?;
    let templated_value = decode_text(&templated)?;
    let templated_ok = templated_value == "<p>Ada &amp; Sons owes $1,234.50</p>";
    passed &= templated_ok;
    report.insert("templateExpansion".into(), json!(templated_ok));
    report.insert("templateValue".into(), json!(templated_value));

    // A template error must surface with desktop runtime's `Failed to render template:`
    // wrapper rather than reaching the renderer as raw markup.
    let template_error = convert(
        app,
        &json!({ "html": "{{x | bogus}}", "format": "inline", "baseName": "Selftest 6" }),
        browser,
    )
    .await
    .err()
    .unwrap_or_default();
    let template_error_ok = template_error
        == "Failed to render template: Unknown template filter \"bogus\" in \"{{x | bogus}}\"";
    passed &= template_error_ok;
    report.insert("templateError".into(), json!(template_error_ok));
    report.insert("templateErrorValue".into(), json!(template_error));

    report.insert("passed".into(), json!(passed));
    Ok(Value::Object(report))
}

fn decode_bytes(result: &Map<String, Value>) -> Result<Vec<u8>, String> {
    let encoded = result
        .get("buffer")
        .and_then(Value::as_str)
        .ok_or_else(|| "result has no base64 buffer".to_string())?;
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| error.to_string())
}

fn decode_text(result: &Map<String, Value>) -> Result<String, String> {
    String::from_utf8(decode_bytes(result)?).map_err(|error| error.to_string())
}

fn filename_is(result: &Map<String, Value>, prefix: &str, extension: &str) -> bool {
    result
        .get("filename")
        .and_then(Value::as_str)
        .map(|name| name.starts_with(prefix) && name.ends_with(&format!(".{extension}")))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A default (A4 portrait, 15mm/12mm margins) layout for the tests, built
    /// the same way the convert pipeline builds it.
    fn default_layout() -> PaperLayout {
        paper_layout(&PdfOptions::default())
    }

    /// The Office media entry must be stored verbatim: deflating an already
    /// compressed JPEG was the dominant cost of a large export.
    #[test]
    fn office_buffer_stores_jpeg_media_verbatim() {
        let mut image = vec![0xffu8, 0xd8];
        image.extend_from_slice(&vec![0u8; 64 * 1024]);
        image.extend_from_slice(&[0xff, 0xd9]);
        let package = office_buffer("docx", &image, &default_layout(), 800, 600).unwrap();
        assert_eq!(&package[..4], b"PK\x03\x04");
        // A deflated entry would have collapsed the 64 KB of zeros; stored, the
        // exact bytes are present in the archive.
        assert!(
            package.windows(image.len()).any(|window| window == image.as_slice()),
            "the JPEG media should appear verbatim in the package"
        );
    }

    #[test]
    fn office_buffer_rejects_non_jpeg() {
        let error = office_buffer("docx", b"not a jpeg", &default_layout(), 800, 600).unwrap_err();
        assert!(error.contains("valid JPEG"), "unexpected error: {error}");
    }
}
