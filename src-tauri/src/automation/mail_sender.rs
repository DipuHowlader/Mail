//! Native Gmail compose, attachment and send flow.
//!
//! All browser actions are expressed through [`BrowserDriver`]. The sender
//! expands one stable set of recipient variables, prepares files before
//! opening Gmail's draft, and reports success only after Gmail adds a new
//! "Message sent" confirmation.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use chrono::{Duration as ChronoDuration, Local};
use regex::{Captures, Regex};
use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

use crate::browser::{BrowserDriver, Key, PageId};

use super::gmail::{
    self, ClipboardPayload, COMPOSE_BODY_SELECTORS, SUBJECT_FIELD_SELECTORS, TO_FIELD_SELECTORS,
};

const GMAIL_ATTACHMENT_LIMIT: u64 = 25 * 1024 * 1024;
const GENERATED_ATTACHMENT_LIMIT: u64 = 100 * 1024;

/// Lowercase substrings that identify Gmail's native "send anyway" dialog
/// ("Send this message without a subject or text in the body?"). The driver
/// accepts every dialog it sees — an unanswered native dialog blocks the
/// renderer — these only mark the expected prompt in the logs.
const SEND_ANYWAY_DIALOG_NEEDLES: &[&str] = &[
    "without a subject",
    "without text in the body",
    "without text in body",
    "send this message",
];

/// Object-safe future, mirroring [`crate::browser::BoxFuture`].
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// One email to send, with everything the sender needs.
///
/// The job-context fields (`session_id`, `campaign_id`, `worker_id`, `slot_id`, `recipient_id`,
/// `index`, `total`) are carried for progress/telemetry and to keep the request self-describing;
/// only `instance_id` is consulted by the current sender.
#[allow(dead_code)]
pub struct SendRequest {
    pub session_id: String,
    pub campaign_id: String,
    pub instance_id: String,
    pub worker_id: String,
    pub slot_id: String,
    pub account_email: String,
    /// The normalized recipient object (`id`, `email`, plus any custom fields).
    pub recipient: Value,
    pub recipient_id: String,
    pub index: usize,
    pub total: usize,
    pub payload: Arc<Value>,
    /// The connected browser and Gmail tab for this instance.
    pub driver: Arc<dyn BrowserDriver>,
    pub page: PageId,
    /// Needed only when the campaign creates a recipient-specific document.
    pub app: Option<AppHandle>,
}

/// Why a send did not happen.
#[derive(Debug)]
pub enum SendFailure {
    /// This recipient failed before Gmail was left with a partially built draft.
    Recipient(String),
    /// Gmail did not confirm the send. Record a failure and continue only after
    /// closing the old compose window so the next recipient gets a fresh draft.
    Uncertain(String),
    /// Stop the worker so another recipient cannot reuse an uncertain draft.
    Stop(String),
}

/// A successful send.
#[derive(Debug, Default)]
pub struct SendReport {
    pub subject: Option<String>,
}

/// Maximum time to wait for Gmail's send confirmation after clicking Send.
/// If the toast or "still sending" state persists past this, the send is
/// marked unconfirmed and the queue proceeds to the next email.
const SEND_CONFIRM_TIMEOUT: Duration = Duration::from_secs(1);

/// Deliver one email.
pub trait MailSender: Send + Sync {
    fn send<'a>(&'a self, request: SendRequest) -> BoxFuture<'a, Result<SendReport, SendFailure>>;
}

/// Tracks the last known state of a send attempt so retries never
/// re-send an email that was already accepted but whose confirmation
/// arrived after the fast-path window closed.
#[derive(Clone, Debug)]
enum SendStatus {
    InProgress,
    Resolved { state: String },
}

/// The production sender. Holds a process-wide map of recent send
/// attempt keys → status for dedup on retries.
pub struct ChromiumMailSender {
    send_status: Arc<Mutex<HashMap<String, SendStatus>>>,
}

impl ChromiumMailSender {
    pub fn new() -> Self {
        Self {
            send_status: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl MailSender for ChromiumMailSender {
    fn send<'a>(&'a self, request: SendRequest) -> BoxFuture<'a, Result<SendReport, SendFailure>> {
        Box::pin(async move {
            // Gmail raises a native `confirm()` ("Send this message without a
            // subject or text in the body?") when the message is empty. A native
            // dialog is drawn by the browser chrome and blocks the renderer's
            // JavaScript, so no injected script can answer it. Arm the driver's
            // auto-accept watcher for the whole send — compose, the Send click
            // and the post-timeout recovery — and let the guard stop it at the
            // end of this block.
            let _dialog_watch = match request
                .driver
                .watch_javascript_dialogs(request.page, SEND_ANYWAY_DIALOG_NEEDLES)
                .await
            {
                Ok(watch) => Some(watch),
                Err(error) => {
                    log::warn!("[gmail] could not watch for the send-anyway dialog: {error}");
                    None
                }
            };
            let prepared = match prepare_message(&request).await {
                Ok(prepared) => prepared,
                Err(error) => return Err(SendFailure::Recipient(error)),
            };
            if let Err(error) = ensure_compose_open(&request.driver, request.page).await {
                return Err(SendFailure::Recipient(error));
            }

            if let Err(error) = compose_and_send(&request, &prepared, &self.send_status).await {
                if is_unconfirmed_send(&error) {
                    let error =
                        match recover_after_uncertain_send(&request.driver, request.page).await {
                            Ok(()) => error,
                            Err(recovery) => {
                                format!("{error} The old compose could not be closed: {recovery}")
                            }
                        };
                    return Err(SendFailure::Uncertain(error));
                }
                return Err(SendFailure::Stop(error));
            }

            Ok(SendReport {
                subject: Some(prepared.subject),
            })
        })
    }
}

fn is_unconfirmed_send(error: &str) -> bool {
    error.contains("Gmail did not confirm whether this message was accepted")
        || error.contains("Timed out waiting for Gmail's send confirmation")
        || error.contains("Could not read Gmail's send confirmation:")
}

/// Close and save any compose window left behind by an unconfirmed send. This
/// lets the next recipient start from a clean compose instead of reusing the
/// uncertain message. Both browser calls are bounded because the renderer may
/// be the source of the timeout.
pub async fn recover_after_uncertain_send(
    driver: &Arc<dyn BrowserDriver>,
    page: PageId,
) -> Result<(), String> {
    let subjects = selectors_json(SUBJECT_FIELD_SELECTORS);
    let close = format!(
        "(()=>{{const visible=e=>{{if(!e)return false;const r=e.getBoundingClientRect(),s=getComputedStyle(e);return r.width>0&&r.height>0&&s.visibility!=='hidden'&&s.display!=='none}};const subjects={subjects}.flatMap(s=>[...document.querySelectorAll(s)]).filter(visible);const subject=subjects.at(-1);if(!subject)return true;const root=subject.closest('[role=dialog]')||document;const controls=[...root.querySelectorAll('button,[role=button],[aria-label],[data-tooltip],[title]')];const label=e=>[e.getAttribute('aria-label'),e.getAttribute('data-tooltip'),e.getAttribute('title')].filter(Boolean).join(' ').replace(/\\s+/g,' ').trim().toLowerCase();const close=controls.find(e=>visible(e)&&['save & close','close'].includes(label(e)));if(!close)return false;close.click();return true}})()"
    );
    let closed = format!(
        "(()=>{{const visible=e=>{{if(!e)return false;const r=e.getBoundingClientRect(),s=getComputedStyle(e);return r.width>0&&r.height>0&&s.visibility!=='hidden'&&s.display!=='none}};return !{subjects}.flatMap(s=>[...document.querySelectorAll(s)]).some(visible)}})()"
    );
    let clicked = matches!(
        tokio::time::timeout(Duration::from_secs(2), driver.evaluate(page, &close)).await,
        Ok(Ok(Value::Bool(true)))
    );
    if clicked
        && matches!(
            tokio::time::timeout(
                Duration::from_secs(3),
                driver.wait_for_function(page, &closed, 2_500, 50),
            )
            .await,
            Ok(Ok(true))
        )
    {
        return Ok(());
    }

    // If Gmail disabled the close control while it was still sending, return
    // to the inbox so the next loop iteration cannot overwrite that composer.
    tokio::time::timeout(
        Duration::from_secs(6),
        driver.goto(page, "https://mail.google.com/mail/u/0/#inbox"),
    )
    .await
    .map_err(|_| "timed out returning Gmail to the inbox".to_string())??;
    let did_close = tokio::time::timeout(
        Duration::from_secs(7),
        driver.wait_for_function(page, &closed, 6_000, 100),
    )
    .await
    .map_err(|_| "timed out waiting for Gmail's composer to close".to_string())??;
    if did_close {
        Ok(())
    } else {
        Err("Gmail kept the compose window open after returning to the inbox.".to_string())
    }
}

struct PreparedMessage {
    recipient: String,
    subject: String,
    text: String,
    rich_html: Option<ClipboardPayload>,
    attachments: Vec<PreparedAttachment>,
    _temporary_files: TemporaryFiles,
}

struct PreparedAttachment {
    path: PathBuf,
    filename: String,
}

#[derive(Default)]
struct TemporaryFiles {
    directory: Option<PathBuf>,
}

impl TemporaryFiles {
    fn directory(&mut self) -> Result<PathBuf, String> {
        if let Some(directory) = &self.directory {
            return Ok(directory.clone());
        }
        let directory = std::env::temp_dir().join(format!(
            "novamail-send-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir(&directory)
            .map_err(|error| format!("Could not prepare temporary email attachments: {error}"))?;
        self.directory = Some(directory.clone());
        Ok(directory)
    }
}

impl Drop for TemporaryFiles {
    fn drop(&mut self) {
        if let Some(directory) = self.directory.take() {
            let _ = fs::remove_dir_all(directory);
        }
    }
}

async fn prepare_message(request: &SendRequest) -> Result<PreparedMessage, String> {
    let recipient = request
        .recipient
        .get("email")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if !recipient.contains('@') {
        return Err("The recipient address is missing or invalid.".to_string());
    }

    let subject_template = request
        .payload
        .get("subjectTemplate")
        .or_else(|| request.payload.get("subject"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let body_template = request
        .payload
        .get("html")
        .and_then(Value::as_str)
        .unwrap_or("");
    let inline_template = request
        .payload
        .get("inlineHtml")
        .and_then(Value::as_str)
        .unwrap_or("");
    let template_source = if inline_template.trim().is_empty() {
        body_template
    } else {
        inline_template
    };
    let text_template = request
        .payload
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("");

    let context = recipient_context(
        request.payload.get("variables"),
        &request.recipient,
        &recipient,
        &request.account_email,
        &[subject_template, template_source, text_template],
    );
    let subject = crate::template::parse_template(subject_template, &context)?
        .replace('\r', " ")
        .replace('\n', " ")
        .trim()
        .to_string();
    let rendered_source = crate::template::parse_template(template_source, &context)?;
    let mut text = crate::template::parse_template(text_template, &context)?;
    text = text.split_whitespace().collect::<Vec<_>>().join(" ");

    let output_format = request
        .payload
        .get("outputFormat")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let inline_output = gmail::is_inline_html_format(&output_format);
    // A document output (pdf/docx/xlsx/pptx/text) ships the HTML template as an
    // attached *file*. The body and the file are mutually exclusive: the HTML
    // must not be pasted into the body as well, or the recipient sees the
    // document twice. The inline HTML (email body) output is the opposite —
    // the HTML *is* the body and no file is attached (see `prepare_attachments`).
    let document_output = generated_document_format(&output_format);

    // Only the inline HTML output (or a legacy caller that selected no output
    // format at all) injects the HTML into the email body. For a document
    // output the body falls back to the plain "Email body" text, so the
    // message is the file plus whatever short note the operator typed.
    let wants_rich_body = !document_output && !rendered_source.trim().is_empty();
    let rich_source = if wants_rich_body {
        // The email body should use the reading pane's full width: convert
        // the template's fixed-width main containers to the responsive
        // width:100%/max-width pattern before computed styles are inlined,
        // then re-fluidify the widths the inliner baked as window pixels.
        let fluid_source = crate::template::responsive_container_html(&rendered_source);
        let inlined = inline_email_styles(request, &fluid_source).await?;
        crate::template::clamp_fluid_inline_widths(&inlined)
    } else {
        String::new()
    };
    let rich_html = if wants_rich_body && !rich_source.trim().is_empty() {
        match gmail::sanitize_email_html(&rich_source) {
            // A rich body is mandatory for the inline formats: the old path
            // failed the send and callers expect that contract to stand.
            Err(error) if inline_output => return Err(error),
            // For document formats the rich body is a bonus over the plain
            // text body, so a sanitization failure degrades instead of
            // aborting a send that used to work.
            Err(error) => {
                log::debug!(
                    "[mail] HTML body withheld for a document-format send ({error}); using the plain text body."
                );
                None
            }
            Ok(sanitized) if sanitized.is_empty() => None,
            Ok(sanitized)
                if inline_output && sanitized.text.trim().is_empty() =>
            {
                return Err("The inline HTML template has no insertable email text.".to_string());
            }
            Ok(mut payload) => {
                // Push the same invisible marker onto the rich payload so the pasted
                // HTML body is byte-unique too, exactly like the plain-text path.
                gmail::append_invisible_body_marker(&mut payload);
                Some(payload)
            }
        }
    } else {
        None
    };

    // The per-recipient document: only the formats that promise a *file*
    // (pdf/docx/xlsx/pptx/text) are ever generated and attached. An inline
    // format can never produce an attachment, which is what stopped the HTML
    // body itself from riding into the mail as a generated `.html` file.
    let auto_generate = request
        .payload
        .get("autoGenerateFile")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let generate_document = !inline_template.trim().is_empty()
        && (auto_generate || (!output_format.is_empty() && !inline_output))
        && generated_document_format(&output_format);

    let mut temporary_files = TemporaryFiles::default();
    let attachments = prepare_attachments(
        request,
        &context,
        if generate_document {
            output_format.as_str()
        } else {
            ""
        },
        &rendered_source,
        &mut temporary_files,
    )
    .await?;

    Ok(PreparedMessage {
        recipient,
        subject,
        text,
        rich_html,
        attachments,
        _temporary_files: temporary_files,
    })
}

/// Resolve `<style>`, stylesheet links and the trusted Tailwind Play CDN into
/// inline declarations before the Gmail HTML sanitizer drops classes/styles.
async fn inline_email_styles(request: &SendRequest, html: &str) -> Result<String, String> {
    let lower_html = html.to_ascii_lowercase();
    let has_styles = lower_html.contains("<style")
        || (lower_html.contains("<link") && lower_html.contains("stylesheet"));
    let has_tailwind = lower_html.contains("cdn.tailwindcss.com");
    if !has_styles && !has_tailwind {
        return Ok(html.to_string());
    }

    let app = request.app.as_ref().ok_or_else(|| {
        "Could not resolve inline email styles because the app session was lost.".to_string()
    })?;
    let browser = app.state::<crate::browser::BrowserService>();
    let driver = browser.driver().await?;
    let page = driver.new_page(None).await?;
    let rendered = async {
        let safe_source = safe_style_source(html);
        let source = format!(
            "<!doctype html><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; img-src data: http: https:; style-src 'unsafe-inline' http: https:; font-src data: http: https:; script-src https://cdn.tailwindcss.com 'unsafe-eval'; connect-src https:\">{safe_source}"
        );
        driver.set_content(page, &source).await?;
        let ready = "document.readyState === 'complete'";
        let _ = driver.wait_for_function(page, ready, 8_000, 100).await?;
        if has_tailwind {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        let properties = json!([
            "display", "visibility", "position", "top", "right", "bottom", "left", "z-index",
            "box-sizing", "width", "min-width", "max-width", "height", "min-height", "max-height",
            "margin", "margin-top", "margin-right", "margin-bottom", "margin-left",
            "padding", "padding-top", "padding-right", "padding-bottom", "padding-left",
            "font", "font-family", "font-size", "font-weight", "font-style", "line-height",
            "font-variant", "letter-spacing", "text-align", "text-align-last", "text-decoration",
            "text-transform", "text-shadow", "white-space", "color", "background", "background-color",
            "background-image", "background-position", "background-repeat", "background-size", "border",
            "border-width", "border-style", "border-color", "border-radius", "border-collapse",
            "border-spacing", "box-shadow", "opacity", "overflow", "vertical-align", "list-style",
            "flex", "flex-direction", "flex-wrap", "flex-grow", "flex-shrink", "flex-basis",
            "align-items", "align-content", "align-self", "justify-content", "gap",
            "grid-template-columns", "grid-template-rows", "grid-column", "grid-row", "object-fit",
            "list-style-type", "text-indent", "word-break", "overflow-wrap"
        ])
        .to_string();
        let extract = format!(
            "(()=>{{const properties={properties};const bodyProperties=new Set(['margin','margin-top','margin-right','margin-bottom','margin-left','padding','padding-top','padding-right','padding-bottom','padding-left','font-family','font-size','font-weight','font-style','font-variant','line-height','letter-spacing','text-align','color','background','background-color','background-image','background-position','background-repeat','background-size']);const body=document.body;if(!body)return '';for(const element of [body,...body.querySelectorAll('*')]){{const style=getComputedStyle(element);const names=element===body?properties.filter(p=>bodyProperties.has(p)):properties;const value=names.map(p=>{{const v=style.getPropertyValue(p);return v?`${{p}}:${{v}}`:''}}).filter(Boolean).join(';');if(value)element.setAttribute('style',value)}}document.querySelectorAll('script,style,link[rel~=\"stylesheet\" i]').forEach(e=>e.remove());const wrapper=document.createElement('div');wrapper.setAttribute('style',body.getAttribute('style')||'');wrapper.innerHTML=body.innerHTML;return wrapper.outerHTML}})()"
        );
        let result = driver.evaluate(page, &extract).await?;
        result
            .as_str()
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
            .ok_or_else(|| "External HTML styles produced an empty email body.".to_string())
    }
    .await;
    let _ = driver.close_page(page).await;
    rendered
}

fn safe_style_source(html: &str) -> String {
    let csp_meta = regex_csp_meta();
    let without_policy = csp_meta.replace_all(html, "");
    let scripts = regex_script_block();
    let source_attribute = regex_script_source();
    let mut trusted_scripts = Vec::new();
    let without_script_blocks =
        scripts.replace_all(&without_policy, |captures: &regex::Captures<'_>| {
            let attributes = captures.get(1).map(|value| value.as_str()).unwrap_or("");
            let source = source_attribute
                .captures(attributes)
                .and_then(|source| {
                    source
                        .get(1)
                        .or_else(|| source.get(2))
                        .or_else(|| source.get(3))
                })
                .map(|source| source.as_str());
            let Some(source) = source else {
                return String::new();
            };
            let Ok(url) = url::Url::parse(source) else {
                return String::new();
            };
            if url.scheme() != "https"
                || url.host_str() != Some("cdn.tailwindcss.com")
                || url.query().is_some()
                || !regex_tailwind_path().is_match(url.path())
            {
                return String::new();
            }
            trusted_scripts.push(format!("<script src=\"{}\"></script>", url.as_str()));
            String::new()
        });
    let self_closing = regex_self_closing_script();
    let without_self_closing = self_closing.replace_all(&without_script_blocks, "");
    let unclosed = regex_unclosed_script();
    let safe_markup = unclosed.replace_all(&without_self_closing, "").into_owned();
    if trusted_scripts.is_empty() {
        return safe_markup;
    }
    let scripts = trusted_scripts.join("");
    let body_end = regex_body_close();
    if body_end.is_match(&safe_markup) {
        body_end
            .replacen(&safe_markup, 1, format!("{scripts}</body>"))
            .into_owned()
    } else {
        format!("{safe_markup}{scripts}")
    }
}

/// The style-inlining regexes are compiled once; `safe_style_source` runs for
/// every inline-format email, so recompiling them per recipient was wasteful.
fn regex_csp_meta() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?is)<meta\b[^>]*http-equiv\s*=\s*(?:"content-security-policy"|'content-security-policy'|content-security-policy)[^>]*>"#,
        )
        .expect("static CSP meta regex")
    })
}

fn regex_script_block() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?is)<script\b([^>]*)>[\s\S]*?</script\s*>")
            .expect("static script block regex")
    })
}

fn regex_script_source() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?is)\bsrc\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#)
            .expect("static script source regex")
    })
}

fn regex_tailwind_path() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^/(?:\d+\.\d+\.\d+)?/?$").expect("static Tailwind path regex"))
}

fn regex_self_closing_script() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?is)<script\b[^>]*/>").expect("static self-closing script regex")
    })
}

fn regex_unclosed_script() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?is)<script\b[^>]*>[\s\S]*$").expect("static unclosed script regex")
    })
}

fn regex_body_close() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)</body\s*>").expect("static body close regex"))
}

fn regex_number_token() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\$number(\d+)").expect("static number token regex"))
}

fn regex_filename_tags() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\$([A-Za-z_][A-Za-z0-9_]*)")
            .expect("the static filename placeholder regex is valid")
    })
}

fn recipient_context(
    variables: Option<&Value>,
    recipient: &Value,
    email: &str,
    sender_email: &str,
    templates: &[&str],
) -> Value {
    let mut context = variables
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let company_name = context
        .get("companyName")
        .or_else(|| context.get("company_name"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if let Some(fields) = recipient.as_object() {
        context.extend(fields.clone());
    }
    let due_date = (Local::now() + ChronoDuration::days(30))
        .format("%b %-d, %Y")
        .to_string();
    context.insert("companyName".to_string(), json!(company_name.clone()));
    context.insert("company_name".to_string(), json!(company_name));
    context.insert("due_date".to_string(), json!(due_date));
    context.insert("sender_email".to_string(), json!(sender_email));
    context.insert("email".to_string(), json!(email));
    context.insert("date".to_string(), json!(Local::now().to_rfc3339()));
    context.insert("digi_no".to_string(), json!(random_digits(10)));
    context.insert(
        "invoice_no".to_string(),
        json!(format!("INV-{}", random_digits(8))),
    );
    context.insert("number6".to_string(), json!(random_digits(6)));

    for (key, digits) in [("order_id", 8), ("transaction_id", 12), ("customer_id", 10)] {
        let missing = context
            .get(key)
            .map(|value| value.is_null() || value.as_str().map(str::trim).unwrap_or("").is_empty())
            .unwrap_or(true);
        if missing {
            context.insert(key.to_string(), json!(random_digits(digits)));
        }
    }

    let number_token = regex_number_token();
    for template in templates {
        for captures in number_token.captures_iter(template) {
            let Some(length) = captures
                .get(1)
                .and_then(|value| value.as_str().parse::<usize>().ok())
            else {
                continue;
            };
            let key = format!("number{length}");
            context
                .entry(key)
                .or_insert_with(|| json!(random_digits(length.min(64))));
        }
    }

    Value::Object(context)
}

fn random_digits(length: usize) -> String {
    let mut result = String::with_capacity(length);
    while result.len() < length {
        for byte in uuid::Uuid::new_v4().as_bytes() {
            result.push(char::from(b'0' + byte % 10));
            if result.len() == length {
                break;
            }
        }
    }
    result
}

/// The output formats that produce a *file* worth attaching. An inline format
/// is deliberately absent: the HTML template is the email body, and generating
/// an `.html` document from it is what produced the stray attachments.
const GENERATED_DOCUMENT_FORMATS: &[&str] = &["pdf", "docx", "xlsx", "pptx", "text"];

/// `true` when `format` names a document file the sender may generate.
fn generated_document_format(format: &str) -> bool {
    GENERATED_DOCUMENT_FORMATS.contains(&format.trim().to_ascii_lowercase().as_str())
}

async fn prepare_attachments(
    request: &SendRequest,
    context: &Value,
    generated_format: &str,
    rendered_html: &str,
    temporary_files: &mut TemporaryFiles,
) -> Result<Vec<PreparedAttachment>, String> {
    let mut attachments = Vec::new();
    let mut filenames = HashSet::new();
    if !generated_format.is_empty() {
        let base_template = request
            .payload
            .get("fileNameTemplate")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("Invoice_$invoice_no_$username_$customer_id");
        let base_name = render_file_name_pattern(base_template, context)?;
        let app = request.app.as_ref().ok_or_else(|| {
            "Document generation is unavailable because the app session was lost.".to_string()
        })?;
        let browser = app.state::<crate::browser::BrowserService>();
        let document_payload = json!({
            "html": rendered_html,
            "format": generated_format.trim().to_ascii_lowercase(),
            "variables": context,
            "baseName": base_name,
            "includePreview": false,
            "saveToDisk": false,
        });
        let document = crate::documents::convert(app, &document_payload, browser.inner()).await?;
        let filename = document
            .get("filename")
            .and_then(Value::as_str)
            .ok_or_else(|| "The generated attachment did not include a filename.".to_string())?;
        let encoded = document
            .get("buffer")
            .and_then(Value::as_str)
            .ok_or_else(|| "The generated attachment did not include file data.".to_string())?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|error| format!("The generated attachment data was invalid: {error}"))?;
        if bytes.is_empty() {
            return Err(format!("Generated attachment \"{filename}\" is empty."));
        }
        if bytes.len() as u64 > GENERATED_ATTACHMENT_LIMIT {
            return Err(format!(
                "Generated attachment \"{filename}\" exceeds the 100 KB maximum after optimization."
            ));
        }
        let filename = safe_filename(filename)?;
        reserve_filename(&mut filenames, &filename)?;
        let directory = temporary_files.directory()?;
        let path = directory.join(&filename);
        fs::write(&path, &bytes).map_err(|error| {
            format!("Could not write generated attachment \"{filename}\": {error}")
        })?;
        attachments.push(PreparedAttachment { path, filename });
    }

    // Picked files ride along with every send, whatever the body format is —
    // an inline HTML body and attachments are not mutually exclusive. HTML
    // files are skipped because the attach picker loads them into the editor;
    // they are sources, never attachments.
    if let Some(values) = request.payload.get("attachments").and_then(Value::as_array) {
        for value in values {
            let Some(object) = value.as_object() else {
                continue;
            };
            let Some(path) = object
                .get("path")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|path| !path.is_empty())
            else {
                continue;
            };
            let requested_name = object
                .get("filename")
                .and_then(Value::as_str)
                .unwrap_or_else(|| {
                    Path::new(path)
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("file")
                });
            let extension = Path::new(requested_name)
                .extension()
                .and_then(|extension| extension.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if matches!(extension.as_str(), "html" | "htm") {
                continue;
            }
            if object
                .get("tooLarge")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                return Err(format!(
                    "Attachment \"{requested_name}\" exceeds Gmail's 25 MB limit."
                ));
            }
            add_file_attachment(
                &mut attachments,
                &mut filenames,
                temporary_files,
                Path::new(path),
                requested_name,
                false,
            )?;
        }
    }
    Ok(attachments)
}

/// Rust equivalent of `renderFileNamePattern()` in
/// `desktop-runtime/templateEngine.cjs`. It removes separators attached to optional
/// fields that have no value, while leaving unknown placeholders visible.
fn render_file_name_pattern(pattern: &str, context: &Value) -> Result<String, String> {
    let pattern = if pattern.trim().is_empty() {
        "$email"
    } else {
        pattern
    };
    let tags = regex_filename_tags();
    let normalized = tags.replace_all(pattern, |captures: &Captures<'_>| {
        let raw_key = &captures[1];
        let mut key = raw_key.to_string();
        let mut separators = String::new();
        while key.ends_with('_') && !has_filename_context_value(context, &key) {
            key.pop();
            separators.push('_');
        }
        if key == raw_key {
            return captures[0].to_string();
        }

        let generated = matches!(
            key.as_str(),
            "username"
                | "sender_username"
                | "eaddress"
                | "date"
                | "digi_no"
                | "invoice_no"
                | "order_id"
                | "transaction_id"
                | "customer_id"
        ) || key
            .strip_prefix("number")
            .map(|suffix| !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit()))
            .unwrap_or(false);
        if !has_filename_context_value(context, &key) && !generated {
            captures[0].to_string()
        } else {
            format!("${key}{separators}")
        }
    });
    crate::template::parse_template(normalized.as_ref(), context)
}

fn has_filename_context_value(context: &Value, key: &str) -> bool {
    context
        .get(key)
        .filter(|value| !value.is_null())
        .map(|value| !filename_context_string(value).trim().is_empty())
        .unwrap_or(false)
}

fn filename_context_string(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => value.clone(),
        Value::Array(values) => values
            .iter()
            .map(|value| match value {
                Value::Null => String::new(),
                Value::Array(_) | Value::Object(_) => filename_context_string(value),
                other => filename_context_string(other),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

fn add_file_attachment(
    attachments: &mut Vec<PreparedAttachment>,
    filenames: &mut HashSet<String>,
    temporary_files: &mut TemporaryFiles,
    source: &Path,
    requested_name: &str,
    generated: bool,
) -> Result<(), String> {
    let metadata = fs::metadata(source)
        .map_err(|_| format!("Attachment \"{requested_name}\" is no longer available."))?;
    if !metadata.is_file() {
        return Err(format!("Attachment \"{requested_name}\" is not a file."));
    }
    if metadata.len() == 0 {
        return Err(format!("Attachment \"{requested_name}\" is empty."));
    }
    if metadata.len() > GMAIL_ATTACHMENT_LIMIT {
        return Err(format!(
            "Attachment \"{requested_name}\" exceeds Gmail's 25 MB limit."
        ));
    }
    if generated && metadata.len() > GENERATED_ATTACHMENT_LIMIT {
        return Err(format!(
            "Generated attachment \"{requested_name}\" exceeds the 100 KB maximum after optimization."
        ));
    }
    fs::File::open(source)
        .map_err(|_| format!("Attachment \"{requested_name}\" is not readable."))?;

    let filename = safe_filename(requested_name)?;
    reserve_filename(filenames, &filename)?;
    let source_name = source
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let path = if source_name == filename {
        source.to_path_buf()
    } else {
        let directory = temporary_files.directory()?;
        let destination = directory.join(&filename);
        fs::copy(source, &destination)
            .map_err(|error| format!("Could not stage attachment \"{filename}\": {error}"))?;
        destination
    };
    attachments.push(PreparedAttachment { path, filename });
    Ok(())
}

fn safe_filename(requested: &str) -> Result<String, String> {
    let leaf = requested.replace('\\', "/");
    let leaf = leaf.rsplit('/').next().unwrap_or("").trim();
    let mut safe = String::with_capacity(leaf.len());
    for character in leaf.chars() {
        safe.push(
            if matches!(
                character,
                '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
            ) {
                '_'
            } else {
                character
            },
        );
    }
    if safe.is_empty() || safe == "." || safe == ".." {
        return Err("Attachment has an invalid filename.".to_string());
    }
    Ok(safe)
}

fn reserve_filename(filenames: &mut HashSet<String>, filename: &str) -> Result<(), String> {
    if !filenames.insert(filename.to_ascii_lowercase()) {
        return Err(format!("More than one attachment is named \"{filename}\"."));
    }
    Ok(())
}

async fn ensure_compose_open(driver: &Arc<dyn BrowserDriver>, page: PageId) -> Result<(), String> {
    let host = driver
        .evaluate(page, "location.hostname.toLowerCase()")
        .await?
        .as_str()
        .unwrap_or("")
        .to_string();
    if host != "mail.google.com" {
        return Err(format!(
            "Gmail compose was not opened because the browser is on {}, not mail.google.com.",
            if host.is_empty() {
                "an unknown page"
            } else {
                &host
            }
        ));
    }

    let probe = driver.evaluate(page, &compose_probe_script()).await?;
    let is_ready = probe.get("ready").and_then(Value::as_bool) == Some(true);
    if is_ready {
        return Ok(());
    }
    // A composer can briefly expose its subject field before Gmail finishes
    // mounting the body editor. Wait only in that state; a closed composer can
    // go straight to the Compose button without paying a fixed polling delay.
    let opening = probe.get("opening").and_then(Value::as_bool) == Some(true);
    let ready = compose_ready_script();
    if opening && driver.wait_for_function(page, &ready, 1_000, 50).await? {
        return Ok(());
    }

    // A single DOM click can be swallowed while Gmail is still wiring up its
    // handlers (the inbox selector matches before the UI is interactive), which
    // leaves the page with no compose at all. Retry the click a few times before
    // concluding that compose did not open.
    let mut last_error = String::new();
    for attempt in 0..3 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(700)).await;
        }
        match click_compose_button(driver, page).await {
            Ok(()) => {}
            Err(error) => {
                last_error = error;
                continue;
            }
        }
        if driver.wait_for_function(page, &ready, 6_000, 100).await? {
            return Ok(());
        }
    }

    // The click did not open an editor. Gmail exposes the same composer as a
    // deep link (`#inbox?compose=new`); driving it is independent of the button's
    // event wiring, so try it once before giving up. A same-document hash change
    // (not a reload) keeps the existing session.
    if last_error.is_empty() {
        let deep_link = "https://mail.google.com/mail/u/0/#inbox?compose=new";
        let _ = driver.goto(page, deep_link).await;
        if driver.wait_for_function(page, &ready, 15_000, 100).await? {
            return Ok(());
        }
    }

    // Nothing worked. Capture what the page actually contains, plus every open
    // browser target: an account set to "Compose in a new window" opens the
    // editor in a separate window, so the URL list is what distinguishes "the
    // click did nothing" from "compose is in another window".
    let diagnostic = driver
        .evaluate(page, &compose_diagnostic_script())
        .await
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default();
    let pages = driver.page_urls().await.unwrap_or_default();
    Err(format!(
        "Gmail did not open a compose window. Compose DOM: {diagnostic} pages: {pages:?}{}",
        if last_error.is_empty() {
            String::new()
        } else {
            format!(" last click error: {last_error}")
        }
    ))
}

/// Live diagnostic used by the `gmail-live` self-test: reports the page state
/// before and after [`ensure_compose_open`], so a failure can be pinned to a
/// concrete DOM cause without a debugger. Never sends anything.
pub async fn live_compose_probe(driver: &Arc<dyn BrowserDriver>, page: PageId) -> Value {
    let mut report = serde_json::Map::new();
    let url_before = driver
        .evaluate(page, "location.href")
        .await
        .unwrap_or(Value::Null);
    let dom_before = driver
        .evaluate(page, &compose_diagnostic_script())
        .await
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .map(Value::String)
        .unwrap_or(Value::Null);
    report.insert(
        "before".into(),
        json!({ "url": url_before, "dom": dom_before }),
    );
    report.insert(
        "composeButton".into(),
        driver
            .evaluate(page, "(()=>{const visible=e=>{if(!e)return false;const r=e.getBoundingClientRect(),s=getComputedStyle(e);return r.width>0&&r.height>0&&s.visibility!=='hidden'&&s.display!=='none'};const prim=[...document.querySelectorAll('[gh=cm]')];const fb=[...document.querySelectorAll('[role=button][aria-label*=Compose i],button[aria-label*=Compose i]')];return JSON.stringify({readyState:document.readyState,ghCm:prim.length,ghCmVisible:prim.filter(visible).length,fallbackVisible:fb.filter(visible).length})})()")
            .await
            .ok()
            .and_then(|value| value.as_str().map(str::to_string))
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    let result = ensure_compose_open(driver, page).await;
    report.insert("opened".into(), Value::Bool(result.is_ok()));
    if let Err(error) = result {
        report.insert("error".into(), Value::String(error));
    }
    let url_after = driver
        .evaluate(page, "location.href")
        .await
        .unwrap_or(Value::Null);
    let dom_after = driver
        .evaluate(page, &compose_diagnostic_script())
        .await
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .map(Value::String)
        .unwrap_or(Value::Null);
    report.insert(
        "after".into(),
        json!({ "url": url_after, "dom": dom_after }),
    );
    report.insert(
        "pages".into(),
        driver
            .page_urls()
            .await
            .map(|pages| json!(pages))
            .unwrap_or(Value::Null),
    );
    Value::Object(report)
}

/// Times each low-level step of the compose flow against a live compose window,
/// mirroring the production sequence, so a hang can be attributed to one CDP
/// command. Tests both a plain `<input>` (the subject) and the recipient
/// combobox, to separate a general `Input.insertText` problem from one specific
/// to the recipient field.
pub async fn live_ops_probe(driver: &Arc<dyn BrowserDriver>, page: PageId) -> Value {
    let active_script = "(()=>{const a=document.activeElement;return JSON.stringify({tag:a?a.tagName.toLowerCase():null,ce:a?a.getAttribute('contenteditable'):null,label:a?a.getAttribute('aria-label'):null})})()";
    let mut out = serde_json::Map::new();

    out.insert(
        "activeBefore".into(),
        driver
            .evaluate(page, active_script)
            .await
            .unwrap_or(Value::Null),
    );

    // 1. Insert into the subject field (a plain <input>).
    let start = Instant::now();
    let focus_subject = focus_field(driver, page, SUBJECT_FIELD_SELECTORS, false, false).await;
    let subject_insert = if focus_subject.is_ok() {
        driver.insert_text(page, "probe").await
    } else {
        Ok(())
    };
    out.insert(
        "subjectInsert".into(),
        json!({
            "focusOk": focus_subject.is_ok(),
            "ok": subject_insert.is_ok(),
            "error": subject_insert.err(),
            "ms": start.elapsed().as_millis(),
        }),
    );

    // 2. The production recipient sequence, step by step.
    let start = Instant::now();
    let focus = focus_recipient_field(driver, page).await;
    out.insert(
        "focusTo".into(),
        json!({ "ok": focus.is_ok(), "error": focus.err(), "ms": start.elapsed().as_millis() }),
    );
    out.insert(
        "activeAfterFocus".into(),
        driver
            .evaluate(page, active_script)
            .await
            .unwrap_or(Value::Null),
    );

    let start = Instant::now();
    let select_all = driver.press_key(page, Key::SelectAll).await;
    out.insert(
        "selectAll".into(),
        json!({ "ok": select_all.is_ok(), "error": select_all.err(), "ms": start.elapsed().as_millis() }),
    );

    let start = Instant::now();
    let backspace = driver.press_key(page, Key::Backspace).await;
    out.insert(
        "backspace".into(),
        json!({ "ok": backspace.is_ok(), "error": backspace.err(), "ms": start.elapsed().as_millis() }),
    );

    let start = Instant::now();
    let insert = driver.insert_text(page, "probe@example.com").await;
    out.insert(
        "recipientInsert".into(),
        json!({ "ok": insert.is_ok(), "error": insert.err(), "ms": start.elapsed().as_millis() }),
    );
    out.insert(
        "valueAfterInsert".into(),
        driver
            .evaluate(
                page,
                "(()=>{const a=document.activeElement;return a?('value'in a?a.value:(a.textContent||'')):null})()",
            )
            .await
            .unwrap_or(Value::Null),
    );

    Value::Object(out)
}

/// JS that binds `body` to the active compose message body. The message body is
/// the largest visible editable in the compose window, so it is chosen
/// structurally: that keeps working when Gmail localizes `aria-label` and the
/// English body selectors miss. The selector matches remain a fallback. Expects
/// `bs`, `root`, and `visible` in scope; defines `body`.
const BODY_PICK_JS: &str = "const bodyEdits=[...root.querySelectorAll('[contenteditable=\"true\"]')].filter(e=>visible(e)&&!e.closest('[role=combobox]'));bodyEdits.sort((a,b)=>{const ra=a.getBoundingClientRect(),rb=b.getBoundingClientRect();return(rb.width*rb.height)-(ra.width*ra.height)});const body=bodyEdits[0]||bs.flatMap(s=>[...root.querySelectorAll(s)]).filter(e=>visible(e)&&e.isContentEditable).at(-1)||null;";

fn compose_probe_script() -> String {
    let subject = selectors_json(SUBJECT_FIELD_SELECTORS);
    let body = selectors_json(COMPOSE_BODY_SELECTORS);
    format!(
        "(()=>{{const visible=e=>{{if(!e)return false;const r=e.getBoundingClientRect(),s=getComputedStyle(e);return r.width>0&&r.height>0&&s.visibility!=='hidden'&&s.display!=='none'}};const ss={subject},bs={body};const sub=ss.flatMap(s=>[...document.querySelectorAll(s)]).filter(visible).at(-1);const root=sub?.closest('[role=dialog]')||document;const hasBody=bs.flatMap(s=>[...root.querySelectorAll(s)]).some(visible)||bs.flatMap(s=>[...document.querySelectorAll(s)]).some(visible)||[...root.querySelectorAll('[contenteditable=\"true\"]')].some(e=>visible(e)&&!e.closest('[role=combobox]'));return{{ready:!!sub&&hasBody,opening:!!sub&&!hasBody}}}})()"
    )
}

fn compose_ready_script() -> String {
    format!(
        "(()=>{{const state={state};return state.ready}})()",
        state = compose_probe_script()
    )
}

/// Describes the compose-related nodes the readiness predicate looked for, so a
/// failure can be diagnosed from telemetry without a live debugging session.
fn compose_diagnostic_script() -> String {
    let subject = selectors_json(SUBJECT_FIELD_SELECTORS);
    let body = selectors_json(COMPOSE_BODY_SELECTORS);
    format!(
        "(()=>{{const visible=e=>{{if(!e)return false;const r=e.getBoundingClientRect(),s=getComputedStyle(e);return r.width>0&&r.height>0&&s.visibility!=='hidden'&&s.display!=='none'}};const ss={subject},bs={body};const desc=e=>{{const r=e.getBoundingClientRect();return{{tag:e.tagName.toLowerCase(),name:e.getAttribute('name'),label:e.getAttribute('aria-label'),role:e.getAttribute('role'),ged:e.getAttribute('g_editable'),ce:e.getAttribute('contenteditable'),cls:String(e.className||'').slice(0,60),w:Math.round(r.width),h:Math.round(r.height),vis:visible(e)}}}};return JSON.stringify({{lang:document.documentElement.lang,host:location.hostname,hash:String(location.hash).slice(0,80),dialogs:document.querySelectorAll('[role=dialog]').length,subjects:[...document.querySelectorAll(ss.join(','))].slice(-4).map(desc),bodies:[...document.querySelectorAll(bs.join(','))].slice(-4).map(desc),editables:[...document.querySelectorAll('[contenteditable=\"true\"]')].slice(-10).map(desc)}})}})()"
    )
}

async fn click_compose_button(driver: &Arc<dyn BrowserDriver>, page: PageId) -> Result<(), String> {
    let marker = uuid::Uuid::new_v4().simple().to_string();
    let marker_json = json!(marker).to_string();
    // Prefer Gmail's own Compose control (`[gh=cm]`); the aria-label matches are
    // only a fallback for builds that drop the attribute, so a different
    // "Compose"-labelled control can never be clicked by mistake.
    let script = format!(
        "(()=>{{const visible=e=>{{const r=e.getBoundingClientRect(),s=getComputedStyle(e);return r.width>0&&r.height>0&&s.visibility!=='hidden'&&s.display!=='none'}};const primary=[...document.querySelectorAll('[gh=cm]')].filter(visible);const fallback=[...document.querySelectorAll('[role=button][aria-label*=Compose i], button[aria-label*=Compose i]')].filter(visible);const el=(primary.length?primary:fallback)[0];if(!el)return false;el.setAttribute('data-novamail-compose-target',{marker_json});return true}})()"
    );
    let found = driver.evaluate(page, &script).await?;
    if found != Value::Bool(true) {
        return Err("Gmail's Compose button was not found.".to_string());
    }
    let selector = format!("[data-novamail-compose-target=\"{marker}\"]");
    // Drive the control through the full pointer/mouse sequence Gmail listens
    // for, then its own `click()`. A bare `el.click()` (click event only) is
    // swallowed on some Gmail builds whose Compose control opens on
    // `pointerdown`/`mousedown`. Kept as DOM events rather than CDP
    // `Input.dispatchMouseEvent`, which has taken down Gmail renderers.
    let pointer_coords = pointer_point_js("el");
    let click_script = format!(
        "(()=>{{const el=document.querySelector({selector_json});if(!el||!(el instanceof HTMLElement))return false;const r=el.getBoundingClientRect();if(!r.width||!r.height)return false;{pointer_coords}el.focus&&el.focus();const base={{bubbles:true,cancelable:true,composed:true,view:window,button:0,detail:1,{pointer_coord_words}}};const fire=(type,extra)=>{{try{{el.dispatchEvent(type==='pointerdown'||type==='pointerup'?new PointerEvent(type,Object.assign({{}},base,extra)):new MouseEvent(type,Object.assign({{}},base,extra)));}}catch(e){{}}}};fire('pointerdown',{{buttons:1}});fire('mousedown',{{buttons:1}});fire('pointerup',{{buttons:0}});fire('mouseup',{{buttons:0}});el.click();return true}})()",
        selector_json = serde_json::to_string(&selector).unwrap_or_else(|_| "\"\"".to_string()),
        pointer_coords = pointer_coords,
        pointer_coord_words = POINTER_COORD_WORDS,
    );
    let clicked = match driver.evaluate(page, &click_script).await {
        Ok(Value::Bool(true)) => Ok(()),
        Ok(_) => Err("Gmail's Compose button did not respond to the click.".to_string()),
        Err(error) => Err(format!("Could not open Gmail compose: {error}")),
    };
    let cleanup = "(()=>{document.querySelectorAll('[data-novamail-compose-target]').forEach(e=>e.removeAttribute('data-novamail-compose-target'));return true})()".to_string();
    let _ = driver.evaluate(page, &cleanup).await;
    clicked.map_err(|error| format!("Could not open Gmail compose: {error}"))
}

async fn compose_and_send(
    request: &SendRequest,
    message: &PreparedMessage,
    send_status: &Arc<Mutex<HashMap<String, SendStatus>>>,
) -> Result<(), String> {
    let driver = &request.driver;
    let page = request.page;

    // Scope recipient lookup to the active compose window. Gmail's page-wide
    // search box can also match generic combobox selectors, which made the old
    // global scan slow and occasionally focused the wrong control.
    focus_recipient_field(driver, page)
        .await
        .map_err(|error| format!("Opening Gmail's recipient field failed: {error}"))?;
    // Only select-and-delete when something is actually there to replace (a
    // reused draft); an empty To editor pays nothing for the extra keystrokes.
    let has_content = driver
        .evaluate(
            page,
            "(()=>{const a=document.activeElement;if(!a)return false;return String(('value'in a?a.value:(a.innerText||a.textContent))||'').trim().length>0})()",
        )
        .await?
        .as_bool()
        .unwrap_or(false);
    if has_content {
        driver
            .press_key(page, Key::SelectAll)
            .await
            .map_err(|error| format!("Clearing the recipient field failed: {error}"))?;
        driver
            .press_key(page, Key::Backspace)
            .await
            .map_err(|error| format!("Clearing the recipient field failed: {error}"))?;
    }
    // Insert the complete address in one protocol operation: the driver takes the
    // atomic path for bare addresses (key-by-key dwell/flight pauses would burn
    // seconds per message), and Gmail commits this shape reliably via the keyed
    // Enter below.
    driver
        .insert_text(page, &message.recipient)
        .await
        .map_err(|error| format!("Inserting the recipient address failed: {error}"))?;
    driver
        .press_key(page, Key::Enter)
        .await
        .map_err(|error| format!("Committing the recipient address failed: {error}"))?;
    let recipient_committed = recipient_committed_script(&message.recipient);
    if !driver
        .wait_for_function(page, &recipient_committed, 4_000, 50)
        .await?
    {
        return Err("Gmail did not commit the recipient address.".to_string());
    }

    fill_subject(driver, page, &message.subject)
        .await
        .map_err(|error| format!("Filling the subject failed: {error}"))?;
    if let Some(html) = &message.rich_html {
        paste_rich_body(driver, page, html)
            .await
            .map_err(|error| format!("Filling the message body failed: {error}"))?;
    } else {
        fill_text_body(driver, page, &message.text)
            .await
            .map_err(|error| format!("Filling the message body failed: {error}"))?;
    }

    for attachment in &message.attachments {
        upload_attachment(driver, page, attachment)
            .await
            .map_err(|error| format!("Attaching \"{}\" failed: {error}", attachment.filename))?;
    }
    submit_and_confirm(driver, page, send_status)
        .await
        .map_err(|error| format!("Sending the message failed: {error}"))
}

async fn focus_field(
    driver: &Arc<dyn BrowserDriver>,
    page: PageId,
    selectors: &[&str],
    recipient_fallback: bool,
    body_fallback: bool,
) -> Result<(), String> {
    let script = focus_field_script(selectors, recipient_fallback, body_fallback);
    if driver.evaluate(page, &script).await? == Value::Bool(true) {
        return Ok(());
    }
    if recipient_fallback {
        let expand = expand_recipient_row_script();
        let _ = driver.evaluate(page, &expand).await?;
        if driver.wait_for_function(page, &script, 1_500, 25).await? {
            return Ok(());
        }
    }
    Err("A required Gmail compose field was not found.".to_string())
}

async fn focus_recipient_field(
    driver: &Arc<dyn BrowserDriver>,
    page: PageId,
) -> Result<(), String> {
    let focus = recipient_field_script(true);
    if driver.evaluate(page, &focus).await? == Value::Bool(true) {
        return Ok(());
    }

    if driver
        .evaluate(page, &expand_recipient_row_script())
        .await?
        == Value::Bool(true)
    {
        let available = recipient_field_script(false);
        if driver
            .wait_for_function(page, &available, 1_000, 25)
            .await?
        {
            if driver.evaluate(page, &focus).await? == Value::Bool(true) {
                return Ok(());
            }
        }
    }
    Err("Gmail's recipient field was not found in the active compose window.".to_string())
}

fn recipient_field_script(focus: bool) -> String {
    let selectors = selectors_json(TO_FIELD_SELECTORS);
    let action = if focus {
        let coords = pointer_point_js("target");
        format!(
            "target.focus();{coords}try{{target.dispatchEvent(new PointerEvent('pointerdown',{{bubbles:true,composed:true,pointerId:1,pointerType:'mouse',isPrimary:true,button:0,buttons:1,{POINTER_COORD_WORDS}}}))}}catch(e){{}}try{{target.dispatchEvent(new MouseEvent('mousedown',{{bubbles:true,composed:true,button:0,buttons:1,{POINTER_COORD_WORDS}}}))}}catch(e){{}}try{{target.dispatchEvent(new PointerEvent('pointerup',{{bubbles:true,composed:true,pointerId:1,pointerType:'mouse',isPrimary:true,button:0,buttons:0,{POINTER_COORD_WORDS}}}))}}catch(e){{}}try{{target.dispatchEvent(new MouseEvent('mouseup',{{bubbles:true,composed:true,button:0,buttons:0,{POINTER_COORD_WORDS}}}))}}catch(e){{}}try{{target.click()}}catch(e){{}}target.focus();return true;"
        )
    } else {
        "return true;".to_string()
    };
    format!(
        "(()=>{{const ss={selectors};const visible=e=>{{if(!e)return false;const r=e.getBoundingClientRect(),s=getComputedStyle(e);return r.width>0&&r.height>0&&s.visibility!=='hidden'&&s.display!=='none'}};const subjects=[...document.querySelectorAll('input[name=\"subjectbox\"],input[placeholder=\"Subject\"],textarea[name=\"subject\"]')].filter(visible);const subject=subjects.at(-1);const root=subject?.closest('[role=dialog]')||document;const recipientLike=e=>{{if(!e||typeof e.getAttribute!=='function')return false;const label=[e.getAttribute('name'),e.getAttribute('aria-label'),e.getAttribute('placeholder'),e.getAttribute('data-tooltip')].filter(Boolean).join(' ');return /\\b(?:to|recipient|recipients)\\b/i.test(label)||e.getAttribute('peoplekit-id')==='BbVjBd'}};const active=document.activeElement;const activeIsRecipient=active instanceof Element&&root.contains(active)&&active!==subject&&((root!==document&&ss.some(s=>active.matches(s)))||recipientLike(active));let target=activeIsRecipient?active:null;if(!target){{for(const selector of ss){{const matches=[...root.querySelectorAll(selector)].filter(e=>visible(e)&&e!==subject&&(root!==document||recipientLike(e)));if(matches.length){{target=matches.at(-1);break}}}}}}if(!target&&subject){{const top=subject.getBoundingClientRect().top;const candidates=[...root.querySelectorAll('input[type=\"text\"],input:not([type]),textarea,[contenteditable=\"true\"],[role=\"combobox\"]')].filter(e=>visible(e)&&e!==subject&&e.getBoundingClientRect().top<top-1&&!/subject/i.test(e.getAttribute('name')||'')&&e.getAttribute('aria-label')!=='Message Body'&&!e.closest('[aria-label=\"Message Body\"]')&&(root!==document||recipientLike(e)));candidates.sort((a,b)=>a.getBoundingClientRect().top-b.getBoundingClientRect().top);target=candidates[0]||null}}if(!target)return false;if(activeIsRecipient)return true;{action}}})()"
    )
}

fn focus_field_script(selectors: &[&str], recipient_fallback: bool, body_fallback: bool) -> String {
    let selectors = selectors_json(selectors);
    let fallback = if recipient_fallback {
        r#"if(!target){const subjects=[...document.querySelectorAll('input[name="subjectbox"],input[placeholder="Subject"],textarea[name="subject"]')].filter(visible);const subject=subjects.at(-1);const root=subject?.closest('[role="dialog"]')||document;if(subject){const subjectTop=subject.getBoundingClientRect().top;const candidates=[...root.querySelectorAll('input[type="text"],input:not([type]),textarea,[contenteditable="true"],[role="combobox"]')].filter(e=>visible(e)&&e!==subject&&!/subject/i.test(e.getAttribute('name')||'')&&e.getAttribute('aria-label')!=='Message Body'&&!e.closest('[aria-label="Message Body"]')&&e.getBoundingClientRect().top<subjectTop-1);candidates.sort((a,b)=>a.getBoundingClientRect().top-b.getBoundingClientRect().top);target=candidates[0]||null}}"#
    } else if body_fallback {
        // The message body is the largest visible editable in the compose
        // window. Choosing it structurally keeps the body working when Gmail's
        // localized `aria-label` does not match the English selectors; on an
        // English UI the largest editable is the same element the selectors
        // returned, so this does not change the normal path.
        r#"const subjects=[...document.querySelectorAll('input[name="subjectbox"],input[placeholder="Subject"],textarea[name="subject"]')].filter(visible);const root=subjects.at(-1)?.closest('[role="dialog"]')||document;const edits=deepQuery(root,'[contenteditable="true"]').filter(e=>visible(e)&&!e.closest('[role="combobox"]'));edits.sort((a,b)=>{const ra=a.getBoundingClientRect(),rb=b.getBoundingClientRect();return(rb.width*rb.height)-(ra.width*ra.height)});if(edits[0])target=edits[0];"#
    } else {
        ""
    };
    format!(
        "(()=>{{const ss={selectors};const visible=e=>{{if(!e)return false;const r=e.getBoundingClientRect(),s=getComputedStyle(e);return r.width>0&&r.height>0&&s.visibility!=='hidden'&&s.display!=='none'}};const deepQuery=(root,selector)=>{{const result=[...root.querySelectorAll(selector)];for(const e of root.querySelectorAll('*'))if(e.shadowRoot)result.push(...deepQuery(e.shadowRoot,selector));return result}};let target=ss.flatMap(s=>[...document.querySelectorAll(s)]).filter(visible).at(-1)||null;if(!target)target=ss.flatMap(s=>deepQuery(document,s)).filter(visible).at(-1)||null;{fallback}if(!target)return false;{coords}target.focus();try{{target.dispatchEvent(new PointerEvent('pointerdown',{{bubbles:true,composed:true,pointerId:1,pointerType:'mouse',isPrimary:true,button:0,buttons:1,{pointer_coord_words}}}))}}catch(e){{}}try{{target.dispatchEvent(new MouseEvent('mousedown',{{bubbles:true,composed:true,button:0,buttons:1,{pointer_coord_words}}}))}}catch(e){{}}try{{target.dispatchEvent(new PointerEvent('pointerup',{{bubbles:true,composed:true,pointerId:1,pointerType:'mouse',isPrimary:true,button:0,buttons:0,{pointer_coord_words}}}))}}catch(e){{}}try{{target.dispatchEvent(new MouseEvent('mouseup',{{bubbles:true,composed:true,button:0,buttons:0,{pointer_coord_words}}}))}}catch(e){{}}try{{target.click()}}catch(e){{}}try{{target.focus()}}catch(e){{}}return true}})()",
        selectors = selectors,
        fallback = fallback,
        coords = pointer_point_js("target"),
        pointer_coord_words = POINTER_COORD_WORDS,
    )
}

fn expand_recipient_row_script() -> String {
    "(()=>{const visible=e=>{const r=e.getBoundingClientRect(),s=getComputedStyle(e);return r.width>0&&r.height>0&&s.visibility!=='hidden'&&s.display!=='none'};const subjects=[...document.querySelectorAll('input[name=\"subjectbox\"],input[placeholder=\"Subject\"],textarea[name=\"subject\"]')].filter(visible);const root=subjects.at(-1)?.closest('[role=dialog]')||document;const norm=s=>String(s||'').replace(/[\\s\\u00a0]+/g,' ').trim();const items=[...root.querySelectorAll('button,[role=button],[role=link],[tabindex],[aria-label],[data-tooltip]')].filter(visible);const byLabel=items.find(e=>/^to(?: recipients?)?$/i.test(norm(e.getAttribute('aria-label')||e.getAttribute('data-tooltip')||'')));const byText=items.find(e=>/^(to|recipients?)$/i.test(norm(e.innerText||e.textContent||'')));const target=byLabel||byText;if(!target)return false;target.click();return true})()".to_string()
}

fn recipient_committed_script(recipient: &str) -> String {
    let selectors = selectors_json(TO_FIELD_SELECTORS);
    let recipient = json!(recipient.to_ascii_lowercase()).to_string();
    format!(
        "(()=>{{const email={recipient},ss={selectors};const visible=e=>{{const r=e.getBoundingClientRect(),s=getComputedStyle(e);return r.width>0&&r.height>0&&s.visibility!=='hidden'&&s.display!=='none'}};const subjects=[...document.querySelectorAll('input[name=\"subjectbox\"],input[placeholder=\"Subject\"],textarea[name=\"subject\"]')].filter(visible);if(!subjects.length)return false;const root=subjects.at(-1).closest('[role=dialog]')||document;const chip=[...root.querySelectorAll('[email],[data-hovercard-id]')].some(e=>[e.getAttribute('email'),e.getAttribute('data-hovercard-id')].some(v=>v?.toLowerCase()===email));const text=(root.textContent||'').toLowerCase().includes(email);const field=ss.flatMap(s=>[...root.querySelectorAll(s)]).filter(visible).at(-1);const value=field?('value'in field?field.value:field.textContent).trim().toLowerCase():'';return (chip||text)&&value!==email}})()"
    )
}

async fn fill_subject(
    driver: &Arc<dyn BrowserDriver>,
    page: PageId,
    subject: &str,
) -> Result<(), String> {
    focus_field(driver, page, SUBJECT_FIELD_SELECTORS, false, false).await?;
    driver.press_key(page, Key::SelectAll).await?;
    driver.press_key(page, Key::Backspace).await?;
    if !subject.is_empty() {
        // The subject ships as one atomic protocol insert instead of the
        // humanised key-by-key stream: it is a plain <input>, nothing keyed it
        // before humanised typing existed, and the cost is seconds of dwell
        // pauses per message. `fill_subject` verifies the value below either
        // way, so the acceptance contract is unchanged.
        driver.insert_text_fragment(page, subject).await?;
    }
    let subject_json = json!(subject).to_string();
    let selectors = selectors_json(SUBJECT_FIELD_SELECTORS);
    let check = format!(
        "(()=>{{const ss={selectors},expected={subject_json};const visible=e=>{{const r=e.getBoundingClientRect(),s=getComputedStyle(e);return r.width>0&&r.height>0&&s.visibility!=='hidden'&&s.display!=='none'}};const field=ss.flatMap(s=>[...document.querySelectorAll(s)]).filter(visible).at(-1);return !!field&&field.value===expected}})()"
    );
    if !driver.wait_for_function(page, &check, 8_000, 100).await? {
        return Err("Gmail did not accept the subject value.".to_string());
    }
    Ok(())
}

async fn fill_text_body(
    driver: &Arc<dyn BrowserDriver>,
    page: PageId,
    text: &str,
) -> Result<(), String> {
    focus_field(driver, page, COMPOSE_BODY_SELECTORS, false, true).await?;
    driver.press_key(page, Key::SelectAll).await?;
    driver.press_key(page, Key::Backspace).await?;
    let body = {
        // Always append a fresh invisible zero-width marker
        // (`gmail::create_invisible_body_text`) so every outgoing body is
        // byte-unique and invisible to the recipient. The Select-all + Backspace
        // above already cleared whatever the operator typed into the composer,
        // so this body replaces it.
        let marker = gmail::create_invisible_body_text();
        format!(
            "{}{}",
            text.replace("\r\n", "\n").replace('\r', "\n"),
            marker
        )
    };
    driver.insert_text(page, &body).await?;
    if text.trim().is_empty() {
        return Ok(());
    }
    let body_json = json!(text).to_string();
    let subjects = selectors_json(SUBJECT_FIELD_SELECTORS);
    let bodies = selectors_json(COMPOSE_BODY_SELECTORS);
    let body_pick = BODY_PICK_JS;
    let check = format!(
        "(()=>{{const ss={subjects},bs={bodies},expected={body_json};const visible=e=>{{const r=e.getBoundingClientRect(),s=getComputedStyle(e);return r.width>0&&r.height>0&&s.visibility!=='hidden'&&s.display!=='none'}};const subject=ss.flatMap(s=>[...document.querySelectorAll(s)]).filter(visible).at(-1);const root=subject?.closest('[role=dialog]')||document;{body_pick}const norm=s=>String(s||'').replace(/[\\u200b\\u200c\\u200d\\u2060\\ufeff]/g,'').replace(/\\u00a0/g,' ').replace(/\\s+/g,' ').trim();return !!body&&norm(body.innerText||body.textContent)===norm(expected)}})()"
    );
    if !driver.wait_for_function(page, &check, 8_000, 100).await? {
        return Err("Gmail did not accept the compose body.".to_string());
    }
    Ok(())
}

/// A paste event whose `clipboardData` is assembled inside the page itself.
///
/// Gmail consumes the rich HTML exactly as it would from a real Ctrl+V, but the
/// OS clipboard is never opened, snapshotted or restored — whatever the operator
/// copied stays available the whole time a campaign runs. The script returns
/// false when the active element is not the compose body, the DataTransfer API
/// is missing, or the runtime refuses to attach custom clipboard data to a
/// ClipboardEvent; in every such case the caller falls back to the previous
/// native clipboard paste unchanged.
fn synthetic_paste_script(payload: &ClipboardPayload) -> String {
    let html = json!(payload.html).to_string();
    let text = json!(payload.text).to_string();
    // Built with push_str so JavaScript braces and quotes pass through
    // unchanged; only the JSON-escaped html/text are interpolated.
    let mut script = String::new();
    script.push_str("(()=>{");
    script.push_str(&format!("const html={html},text={text};"));
    script.push_str("const target=document.activeElement;");
    script.push_str("if(!target||target.getAttribute('contenteditable')!=='true')return false;");
    script.push_str("try{target.focus()}catch(e){}");
    script.push_str("let data=null;try{data=new DataTransfer()}catch(e){}");
    script.push_str("if(!data||typeof data.setData!=='function')return false;");
    script.push_str("try{data.setData('text/html',html);data.setData('text/plain',text)}catch(e){return false}");
    script.push_str("let event=null;try{event=new ClipboardEvent('paste',{bubbles:true,cancelable:true,clipboardData:data})}catch(e){}");
    // Verify the payload actually attached before dispatching: an empty
    // `clipboardData` would make Gmail paste nothing instead of the email.
    script.push_str("if(!event||!event.clipboardData||event.clipboardData.getData('text/html')!==html)return false;");
    script.push_str("try{target.dispatchEvent(event)}catch(e){return false}");
    script.push_str("return true})()");
    script
}

async fn paste_rich_body(
    driver: &Arc<dyn BrowserDriver>,
    page: PageId,
    payload: &ClipboardPayload,
) -> Result<(), String> {
    if payload.text.trim().is_empty() {
        return Err("The inline HTML template has no insertable email content.".to_string());
    }
    focus_field(driver, page, COMPOSE_BODY_SELECTORS, false, true).await?;
    driver.press_key(page, Key::SelectAll).await?;
    driver.press_key(page, Key::Backspace).await?;

    let subjects = selectors_json(SUBJECT_FIELD_SELECTORS);
    let bodies = selectors_json(COMPOSE_BODY_SELECTORS);
    let body_pick = BODY_PICK_JS;
    let check = format!(
        "(()=>{{const ss={subjects},bs={bodies};const visible=e=>{{const r=e.getBoundingClientRect(),s=getComputedStyle(e);return r.width>0&&r.height>0&&s.visibility!=='hidden'&&s.display!=='none'}};const subject=ss.flatMap(s=>[...document.querySelectorAll(s)]).filter(visible).at(-1);const root=subject?.closest('[role=dialog]')||document;{body_pick}return !!body&&!!(String(body.innerText||body.textContent||'').replace(/\\u00a0/g,' ').trim()||body.querySelector('img'))}})()"
    );

    // Pipeline one (primary): an in-page paste carrying the payload inside its
    // own DataTransfer. Gmail inserts the rich body exactly like a manual
    // Ctrl+V while the OS clipboard is never opened, so the operator's copies
    // survive every send untouched. The same body confirm gates it with a
    // short window; a miss falls through to the native clipboard paste below.
    let dispatched = driver
        .evaluate(page, &synthetic_paste_script(payload))
        .await
        .map(|result| result == Value::Bool(true))
        .unwrap_or(false);
    if dispatched && driver.wait_for_function(page, &check, 2_500, 100).await? {
        return Ok(());
    }
    log::info!("[gmail] in-page paste not confirmed; using the native clipboard paste.");

    // Pipeline two (unchanged fallback): Gmail's composer enforces Trusted
    // Types, so assigning HTML to `innerHTML` is rejected. On Windows, put the
    // payload on the native HTML clipboard and press a real paste; the lease
    // restores the user's clipboard after the paste is confirmed.
    focus_field(driver, page, COMPOSE_BODY_SELECTORS, false, true).await?;
    driver.press_key(page, Key::SelectAll).await?;
    driver.press_key(page, Key::Backspace).await?;

    #[cfg(windows)]
    let _clipboard_lease = super::clipboard::install_html(&payload.html, &payload.text).await?;
    #[cfg(windows)]
    driver.press_key(page, Key::Paste).await?;

    #[cfg(not(windows))]
    {
        let html = json!(payload.html).to_string();
        let inserted = format!(
            "(()=>{{const html={html};const target=document.activeElement;if(!target||target.getAttribute('contenteditable')!=='true')return false;target.focus();try{{if(document.execCommand('insertHTML',false,html)){{target.dispatchEvent(new InputEvent('input',{{bubbles:true,inputType:'insertHTML',data:null}}));return true}}}}catch(e){{}}return false}})()"
        );
        if driver.evaluate(page, &inserted).await? != Value::Bool(true) {
            // Keep sending functional on platforms without the native HTML
            // clipboard integration. This fallback keeps the message readable,
            // though Gmail may not preserve its rich formatting.
            driver.insert_text(page, &payload.text).await?;
        }
    }

    if !driver.wait_for_function(page, &check, 12_000, 100).await? {
        return Err("Gmail did not apply the rich HTML email body.".to_string());
    }
    Ok(())
}

async fn upload_attachment(
    driver: &Arc<dyn BrowserDriver>,
    page: PageId,
    attachment: &PreparedAttachment,
) -> Result<(), String> {
    let marker = uuid::Uuid::new_v4().simple().to_string();
    let marker_json = json!(marker).to_string();
    let subjects = selectors_json(SUBJECT_FIELD_SELECTORS);
    let mark_input = format!(
        "(()=>{{const ss={subjects};const visible=e=>{{const r=e.getBoundingClientRect(),s=getComputedStyle(e);return r.width>0&&r.height>0&&s.visibility!=='hidden'&&s.display!=='none'}};const subject=ss.flatMap(s=>[...document.querySelectorAll(s)]).filter(visible).at(-1);const root=subject?.closest('[role=dialog]')||document;const all=[...document.querySelectorAll('input[type=file]')];const local=all.filter(e=>root.contains(e));const target=local.find(e=>e.name.toLowerCase()==='filedata')||all.filter(e=>e.name.toLowerCase()==='filedata').at(-1)||(local.length===1?local[0]:all.length===1?all[0]:null);if(!target)return false;target.setAttribute('data-novamail-upload-target',{marker_json});return true}})()"
    );
    let input_marked = driver
        .wait_for_function(page, &mark_input, 10_000, 100)
        .await?;
    if !input_marked {
        // Gmail creates the file input lazily on some layouts. Open the
        // attachment control first, then resolve the input again.
        let opened = driver
            .evaluate(page, &click_attachment_button_script())
            .await?;
        if opened != Value::Bool(true)
            || !driver
                .wait_for_function(page, &mark_input, 10_000, 100)
                .await?
        {
            return Err(
                "Gmail attachment input was not found in the active compose window.".to_string(),
            );
        }
    }

    let selector = format!("[data-novamail-upload-target=\"{marker}\"]");
    let upload = driver
        .set_file_input(page, &selector, vec![attachment.path.clone()])
        .await;
    let cleanup = "(()=>{document.querySelectorAll('[data-novamail-upload-target]').forEach(e=>e.removeAttribute('data-novamail-upload-target'));return true})()";
    let _ = driver.evaluate(page, cleanup).await;
    upload.map_err(|error| {
        format!(
            "Gmail rejected attachment \"{}\": {error}",
            attachment.filename
        )
    })?;

    let filename = json!(attachment.filename).to_string();
    let check = format!(
        "(()=>{{const filename={filename};if(location.hostname.toLowerCase()!=='mail.google.com')return false;const ss={subjects};const visible=e=>{{const r=e.getBoundingClientRect(),s=getComputedStyle(e);return r.width>0&&r.height>0&&s.visibility!=='hidden'&&s.display!=='none'}};const subject=ss.flatMap(s=>[...document.querySelectorAll(s)]).filter(visible).at(-1);const root=subject?.closest('[role=dialog]')||document;const text=root.textContent||'';const progress=[...root.querySelectorAll('[role=progressbar],[aria-busy=\"true\"]')].some(visible)||/uploading|upload in progress/i.test(text);const failed=[...document.querySelectorAll('[role=alert],[role=status],[aria-live=assertive]')].some(e=>/there were errors attaching|couldn't attach|could not attach|attachment failed/i.test(e.textContent||''));if(failed||!text.includes(filename)||progress){{delete window.__novamailAttachmentReady;return false}}const now=Date.now(),previous=window.__novamailAttachmentReady;const since=previous?.filename===filename?previous.since:now;window.__novamailAttachmentReady={{filename,since}};return now-since>=700}})()"
    );
    if !driver.wait_for_function(page, &check, 90_000, 150).await? {
        return Err(format!(
            "Gmail did not finish attaching \"{}\" before the upload timeout.",
            attachment.filename
        ));
    }
    Ok(())
}

fn click_attachment_button_script() -> String {
    let coords = pointer_point_js("target");
    format!(
        "(()=>{{const visible=e=>{{if(!e)return false;const r=e.getBoundingClientRect(),s=getComputedStyle(e);return r.width>0&&r.height>0&&s.visibility!=='hidden'&&s.display!=='none'}};const text=e=>[e.getAttribute('aria-label'),e.getAttribute('data-tooltip'),e.getAttribute('title'),e.textContent].filter(Boolean).join(' ').replace(/[\\u200e\\u200f\\u202a-\\u202e]/g,' ').trim();const buttons=[...document.querySelectorAll('[role=button],button,[aria-label],[data-tooltip]')].filter(visible);const target=buttons.find(e=>/^(?:attach(?: files?)?|add attachment|insert files? using drive)\\b/i.test(text(e)));if(!target)return false;{coords}try{{target.dispatchEvent(new PointerEvent('pointerdown',{{bubbles:true,composed:true,pointerId:1,pointerType:'mouse',isPrimary:true,button:0,buttons:1,{POINTER_COORD_WORDS}}}))}}catch(e){{}}try{{target.dispatchEvent(new MouseEvent('mousedown',{{bubbles:true,composed:true,button:0,buttons:1,{POINTER_COORD_WORDS}}}))}}catch(e){{}}try{{target.dispatchEvent(new PointerEvent('pointerup',{{bubbles:true,composed:true,pointerId:1,pointerType:'mouse',isPrimary:true,button:0,buttons:0,{POINTER_COORD_WORDS}}}))}}catch(e){{}}try{{target.dispatchEvent(new MouseEvent('mouseup',{{bubbles:true,composed:true,button:0,buttons:0,{POINTER_COORD_WORDS}}}))}}catch(e){{}}target.click();return true}})()"
    )
}

async fn submit_and_confirm(
    driver: &Arc<dyn BrowserDriver>,
    page: PageId,
    send_status: &Arc<Mutex<HashMap<String, SendStatus>>>,
) -> Result<(), String> {
    let attempt = uuid::Uuid::new_v4().simple().to_string();
    let attempt_json = json!(attempt).to_string();
    // Build this without `format!` so JavaScript braces and quotes pass through
    // unchanged; only the JSON-escaped attempt ID is interpolated.
    // Keep the observer ES5-compatible for Gmail's embedded Chromium context.
    let mut install_observer = String::from("(function(){var attemptId=");
    install_observer.push_str(&attempt_json);
    install_observer.push_str(
        ";var previous=window.__novamailSendAttempt;if(previous){if(previous.observer)previous.observer.disconnect();if(previous.timer)clearTimeout(previous.timer)}\
        var state={attemptId:attemptId,confirmation:'',error:'',result:null,resolve:null,timer:null,observer:null};\
        function visible(e){if(!e)return false;var r=e.getBoundingClientRect(),s=getComputedStyle(e);return r.width>0&&r.height>0&&s.visibility!=='hidden'&&s.display!=='none'}\
        function textOf(n){if(!n)return '';return n.nodeType===3?(n.nodeValue||''):(n.textContent||'')}\
        function finish(status,text){if(state.result)return;state.result={state:status,message:text};if(status==='sent')state.confirmation=text;if(status==='error')state.error=text;if(state.observer)state.observer.disconnect();if(state.timer)clearTimeout(state.timer);if(state.resolve)state.resolve(state.result)}\
        state.finish=finish;\
        function isPromptText(text){var s=text.toLowerCase();return s.indexOf('without a subject')>=0||s.indexOf('without text in the body')>=0||s.indexOf('without text in body')>=0||s.indexOf('send this message')>=0}\
        function confirmPrompt(){var dialogs=document.querySelectorAll('[role=dialog],[role=alertdialog]');for(var i=0;i<dialogs.length;i++){var prompt=dialogs[i];if(!visible(prompt)||!isPromptText(prompt.textContent||''))continue;var buttons=prompt.querySelectorAll('button,[role=button]');for(var j=0;j<buttons.length;j++){var button=buttons[j],label=(button.getAttribute('aria-label')||button.textContent||'').replace(/\\s+/g,' ').trim().toLowerCase();if(visible(button)&&(label==='ok'||label==='send anyway'||label==='yes')){button.click();return true}}}return false}\
        function inspectText(text){var normalized=text.toLowerCase();if(normalized.indexOf('message not sent')>=0||normalized.indexOf('failed to send')>=0||normalized.indexOf(\"couldn't send\")>=0||normalized.indexOf('could not send')>=0||normalized.indexOf('please specify at least one recipient')>=0||normalized.indexOf('sending failed')>=0){finish('error',text)}else if(normalized.indexOf('message sent')>=0){finish('sent',text)}else if(normalized.indexOf('still sending')>=0||normalized.indexOf('still being sent')>=0){finish('timeout',text)}}\
        function inspectSemanticNode(node){if(!node||node.nodeType!==1)return false;var role=node.getAttribute('role'),live=node.hasAttribute('aria-live'),isDialog=role==='dialog'||role==='alertdialog';if(!isDialog&&role!=='alert'&&role!=='status'&&!live)return false;var text=textOf(node);if(text){inspectText(text);if(isPromptText(text))return true}return false}\
        function scanAddedNode(node){if(!node)return false;if(node.nodeType===3){var text=node.nodeValue||'';if(text)inspectText(text);return isPromptText(text)}if(node.nodeType!==1)return false;var shouldCheckPrompt=inspectSemanticNode(node);var matches=node.querySelectorAll('[role=alert],[role=status],[aria-live],[role=dialog],[role=alertdialog]');for(var i=0;i<matches.length&&!state.result;i++)shouldCheckPrompt=inspectSemanticNode(matches[i])||shouldCheckPrompt;if(node.childElementCount<=8){var compactText=node.textContent||'';if(compactText.length<=256){var normalized=compactText.toLowerCase();if(normalized.indexOf('message sent')>=0||normalized.indexOf('message not sent')>=0||normalized.indexOf('failed to send')>=0||normalized.indexOf('couldn\\'t send')>=0||normalized.indexOf('could not send')>=0||normalized.indexOf('sending failed')>=0||normalized.indexOf('still sending')>=0||normalized.indexOf('still being sent')>=0){inspectText(compactText)}if(isPromptText(compactText))shouldCheckPrompt=true}}return shouldCheckPrompt}\
        state.observer=new MutationObserver(function(records){var shouldCheckPrompt=false;for(var i=0;i<records.length&&!state.result;i++){var record=records[i];if(record.type==='characterData'){var changed=textOf(record.target);if(changed)inspectText(changed);if(record.target.parentElement){var parent=record.target.parentElement.closest('[role=dialog],[role=alertdialog]');if(parent&&isPromptText(textOf(parent)))shouldCheckPrompt=true}}else{var nodes=record.addedNodes||[];for(var j=0;j<nodes.length&&!state.result;j++)shouldCheckPrompt=scanAddedNode(nodes[j])||shouldCheckPrompt}}if(!state.result&&shouldCheckPrompt)confirmPrompt()});\
        state.observer.observe(document.body,{subtree:true,childList:true,characterData:true});window.__novamailSendAttempt=state;confirmPrompt();return true})()",
    );
    driver
        .evaluate(page, &install_observer)
        .await
        .map_err(|error| format!("Could not prepare Gmail's send confirmation: {error}"))?;

    // A background task polls the MutationObserver for a delayed
    // confirmation after the fast-path window closes. It never blocks the
    // queue; it only updates the status map for dedup on retries.
    let started = Instant::now();
    let status_key = format!("send-{}", &attempt[..8]);
    let poll_script = format!(
        "(()=>{{var s=window.__novamailSendAttempt;if(!s||s.attemptId!=={attempt_json})return null;if(s.result)return s.result;return null}})()",
        attempt_json = attempt_json,
    );
    {
        let mut status = send_status.lock().map_err(|e| e.to_string())?;
        status.insert(status_key.clone(), SendStatus::InProgress);
    }
    let status_clone = Arc::clone(send_status);
    let poll_script_clone = poll_script.clone();
    let driver_bg = Arc::clone(driver);
    let status_key_bg = status_key.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if started.elapsed() > Duration::from_secs(15) {
                break;
            }
            if let Ok(result) = driver_bg.evaluate(page, &poll_script_clone).await {
                let state = result
                    .get("state")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if state == "sent" || state == "error" || state == "timeout" {
                    let mut status = match status_clone.lock() {
                        Ok(s) => s,
                        Err(_) => break,
                    };
                    status.insert(
                        status_key_bg.clone(),
                        SendStatus::Resolved {
                            state: state.to_string(),
                        },
                    );
                    break;
                }
            }
        }
    });

    let marker_json = json!(attempt).to_string();
    let subjects = selectors_json(SUBJECT_FIELD_SELECTORS);
    let mark_send = format!(
        "(()=>{{const ss={subjects};const visible=e=>{{if(!e)return false;const r=e.getBoundingClientRect(),s=getComputedStyle(e);return r.width>0&&r.height>0&&s.visibility!=='hidden'&&s.display!=='none'}};const subject=ss.flatMap(s=>[...document.querySelectorAll(s)]).filter(visible).at(-1);const editables=[...document.querySelectorAll('[contenteditable=\"true\"]')].filter(e=>visible(e)&&!e.closest('[role=combobox]'));const body=editables.at(-1);const root=(subject&&subject.closest('[role=dialog]'))||(body&&body.closest('[role=dialog]'))||document;const candidates=[...root.querySelectorAll('[role=button],button,[aria-label],[data-tooltip]')];const enabled=e=>visible(e)&&!e.matches(':disabled')&&e.getAttribute('aria-disabled')!=='true';const label=e=>[e.getAttribute('aria-label'),e.getAttribute('data-tooltip'),e.getAttribute('title'),e.textContent].filter(Boolean).join(' ').trim().toLowerCase();const target=candidates.find(e=>enabled(e)&&(e.classList.contains('T-I-atl')||label(e)==='send'||label(e).startsWith('send ')));if(!target)return false;target.setAttribute('data-novamail-send-target',{marker_json});return true}})()"
    );
    let marked = match driver.evaluate(page, &mark_send).await {
        Ok(value) => value == Value::Bool(true),
        Err(error) => {
            clear_send_observer(driver, page, &attempt).await;
            return Err(format!("Could not locate Gmail's Send button: {error}"));
        }
    };
    if !marked {
        clear_send_observer(driver, page, &attempt).await;
        return Err("Gmail Send button was not found in the active compose window.".to_string());
    }

    let selector = format!("[data-novamail-send-target=\"{attempt}\"]");
    // Drive the control through the full pointer/mouse sequence Gmail listens for, then
    // its own `click()` — the same proven chain the Compose button takes — but keep the
    // send separate from the compose click so a click dropped by one never blocks the
    // other. Coordinates are set for every synthetic event; a bare `el.click()` fires at
    // screen origin (0,0), which behavioural collectors read as a non-human hand. A
    // synthetic CDP mouse click on Send is still avoided: see the compose click.
    let selector_json = serde_json::to_string(&selector).unwrap_or_else(|_| "\"\"".to_string());
    let pointer_coords = pointer_point_js("el");
    let click_script = format!(
        "(()=>{{const el=document.querySelector({selector_json});if(!el||!(el instanceof HTMLElement))return false;const r=el.getBoundingClientRect();if(!r.width||!r.height)return false;{pointer_coords}el.focus&&el.focus();const base={{bubbles:true,cancelable:true,composed:true,view:window,button:0,detail:1,{pointer_coord_words}}};const fire=(type,extra)=>{{try{{el.dispatchEvent(type==='pointerdown'||type==='pointerup'?new PointerEvent(type,Object.assign({{}},base,extra)):new MouseEvent(type,Object.assign({{}},base,extra)));}}catch(e){{}}}};fire('pointerdown',{{buttons:1}});fire('mousedown',{{buttons:1}});fire('pointerup',{{buttons:0}});fire('mouseup',{{buttons:0}});el.click();return true}})()",
        selector_json = selector_json,
        pointer_coords = pointer_coords,
        pointer_coord_words = POINTER_COORD_WORDS,
    );
    let click_result = match driver.evaluate(page, &click_script).await {
        Ok(Value::Bool(true)) => Ok(()),
        Ok(_) => Err("Gmail's Send button did not respond to the click.".to_string()),
        Err(error) => Err(error),
    };
    // Brief settle after clicking Send — mirrors the original flow: most
    // toasts land within this window and the confirmation check below then
    // resolves instantly.
    tokio::time::sleep(Duration::from_secs(1)).await;
    // Keep the confirmation wait inside Chromium. Repeated CDP polls while
    // Gmail is sending can overload an already busy renderer and add latency.
    // Cap at 1s — if the toast hasn't appeared by then, mark the send as
    // unconfirmed and let the queue proceed without waiting for the browser.
    // The background poller picks up the final state for dedup on retries.
    let wait_for_confirmation = format!(
        "(()=>{{var state=window.__novamailSendAttempt;if(!state||state.attemptId!=={attempt_json})return null;if(state.result)return state.result;return new Promise(function(resolve){{state.resolve=resolve;state.timer=setTimeout(function(){{state.finish('timeout','')}},1000)}})}})()"
    );
    let confirmation = match tokio::time::timeout(
        SEND_CONFIRM_TIMEOUT,
        driver.evaluate(page, &wait_for_confirmation),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => {
            // Fast-path timed out. Mark unconfirmed and proceed — the
            // background poller will resolve the final state.
            let mut status = match send_status.lock() {
                Ok(s) => s,
                Err(_) => return Err("Could not update send status.".to_string()),
            };
            status.insert(
                status_key.clone(),
                SendStatus::InProgress,
            );
            return Ok(());
        }
    };
    clear_send_observer(driver, page, &attempt).await;

    match confirmation {
        Ok(value) if value.get("state").and_then(Value::as_str) == Some("sent") => {
            // Gmail confirmed the send — the queue moves on immediately.
            Ok(())
        }
        Ok(value) if value.get("state").and_then(Value::as_str) == Some("error") => Err(format!(
            "Gmail reported that the message was not sent: {}",
            value.get("message").and_then(Value::as_str).unwrap_or("")
        )),
        Ok(value) => {
            // A stalled "still sending" composer lands here too: the observer
            // finishes it with the `timeout` state, which is exactly the
            // unconfirmed case.
            let observed = value
                .get("message")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("");
            let observed_detail = if observed.is_empty() {
                String::new()
            } else {
                format!(" Gmail last showed: \"{observed}\".")
            };
            let click_detail = click_result
                .err()
                .map(|error| format!(" The Send click returned an error: {error}."))
                .unwrap_or_default();
            Err(format!(
                "Gmail did not confirm whether this message was accepted.{observed_detail}{click_detail} Marking the recipient failed and moving to the next one so no duplicate is sent automatically. Check the Gmail Sent folder before retrying."
            ))
        }
        Err(error) => Err(format!("Could not read Gmail's send confirmation: {error}")),
    }
}

async fn clear_send_observer(driver: &Arc<dyn BrowserDriver>, page: PageId, attempt: &str) {
    let attempt = json!(attempt).to_string();
    let script = format!(
        "(()=>{{document.querySelectorAll('[data-novamail-send-target]').forEach(function(element){{element.removeAttribute('data-novamail-send-target')}});const state=window.__novamailSendAttempt;if(state&&state.attemptId==={attempt}){{if(state.observer)state.observer.disconnect();if(state.timer)clearTimeout(state.timer);delete window.__novamailSendAttempt}}return true}})()"
    );
    let _ = tokio::time::timeout(Duration::from_secs(2), driver.evaluate(page, &script)).await;
}

fn selectors_json(selectors: &[&str]) -> String {
    serde_json::to_string(selectors).unwrap_or_else(|_| "[]".to_string())
}

/// A jittered pointer point on `element`, computed in-page, for the synthetic DOM
/// pointer events the compose flow dispatches.
///
/// Every `MouseEvent`/`PointerEvent` init left empty fires at `clientX: 0, clientY: 0`
/// — screen origin — and a behavioural collector reads that as a hand that only ever
/// presses at the top-left corner. These coordinates anchor the press to the element's
/// rectangle the way a real click does (with screen-space values derived from the
/// window frame), keeping the events untrusted though exactly as they are today.
const POINTER_COORD_WORDS: &str =
    "clientX:__px,clientY:__py,pageX:__px,pageY:__py,screenX:__sx,screenY:__sy";

fn pointer_point_js(element: &str) -> String {
    format!(
        r#"const __pe=({element}.getBoundingClientRect?{element}.getBoundingClientRect():null)||{{width:0,height:0,left:0,top:0}};const __px=__pe.left+Math.round(__pe.width*(0.42+0.16*Math.random()));const __py=__pe.top+Math.round(__pe.height*(0.42+0.16*Math.random()));const __sx=Math.round(window.screenX+__px);const __sy=Math.round(window.screenY+__py+Math.max(0,window.outerHeight-window.innerHeight));"#
    )
}
