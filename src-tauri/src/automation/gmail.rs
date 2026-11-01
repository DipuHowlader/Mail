//! The Gmail send pipeline: a faithful port of the browser-DOM half of
//! `desktop-runtime/automation.cjs` (`sendOneCore` and its helpers).
//!
//! desktop runtime drove Gmail through Playwright's `page`/`elementHandle` objects.
//! Here the same DOM work goes through [`crate::browser::BrowserDriver`], which
//! exposes only CDP-level primitives (`evaluate`, `insert_text`, `press_key`,
//! `focus`, `click`, `set_file_input`, `wait_for_function`). Element handles
//! become CSS-selector lookups that are re-resolved on each step, which is also
//! what makes the compose flow resilient to Gmail replacing its DOM mid-send.
//!
//! This module is split so the pieces that do not need a live browser can be
//! unit-tested (see [`selftest`]): the selector tables, the `inline`-format
//! helper, the zero-width default body, the URL allow-list and the HTML
//! sanitizer that turns the inlined email markup into Gmail's clipboard
//! payload.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::OnceLock;

use regex::Regex;

use html5ever::tendril::StrTendril;
use html5ever::tokenizer::{
    BufferQueue, CharacterTokens, NullCharacterToken, TagKind, TagToken, Token, TokenSink,
    TokenSinkResult, Tokenizer, TokenizerOpts,
};

/// Formats whose email body is inserted as rich HTML (mirrors
/// `INLINE_HTML_FORMATS`).
const INLINE_HTML_FORMATS: &[&str] = &["inline", "inline-html", "inline-text", "html"];

/// Zero-width characters used to seed an otherwise empty compose body
/// (`ZERO_WIDTH_BODY_CHARACTERS`).
const ZERO_WIDTH_BODY_CHARACTERS: &[char] =
    &['\u{200b}', '\u{200c}', '\u{200d}', '\u{2060}', '\u{feff}'];
const ZERO_WIDTH_BODY_MIN_LENGTH: u32 = 5;
const ZERO_WIDTH_BODY_MAX_LENGTH: u32 = 10;

/// The tag and attribute allow-lists of `parseSafeEmailHtml`.
const EMAIL_HTML_TAGS: &[&str] = &[
    "a",
    "b",
    "blockquote",
    "br",
    "caption",
    "center",
    "div",
    "em",
    "font",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "hr",
    "i",
    "img",
    "li",
    "ol",
    "p",
    "s",
    "span",
    "strike",
    "strong",
    "sub",
    "sup",
    "table",
    "tbody",
    "td",
    "th",
    "thead",
    "tr",
    "u",
    "ul",
];
const EMAIL_HTML_DROP_TAGS: &[&str] = &[
    "iframe", "object", "embed", "script", "style", "title", "form", "input", "button", "textarea",
    "select", "video", "audio",
];
const EMAIL_HTML_ATTRIBUTES: &[&str] = &[
    "align",
    "alt",
    "bgcolor",
    "border",
    "cellpadding",
    "cellspacing",
    "colspan",
    "color",
    "face",
    "height",
    "href",
    "hspace",
    "src",
    "valign",
    "width",
    "vspace",
    "rowspan",
    "style",
    "dir",
    "title",
];

/// HTML void elements: they never have children, so a start tag must not be
/// pushed onto the open-element stack.
const VOID_TAGS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// Tags whose text flavour adds a trailing newline (`htmlTreeToClipboardPayload`).
const TEXT_BREAK_TAGS: &[&str] = &[
    "br", "p", "div", "li", "tr", "h1", "h2", "h3", "h4", "h5", "h6",
];

/// The clipboard payload Gmail receives, matching `htmlTreeToClipboardPayload`.
#[derive(Debug, Default, Clone)]
pub struct ClipboardPayload {
    pub html: String,
    pub text: String,
    pub image_count: usize,
    /// Number of top-level nodes in the sanitized tree. An empty tree (all
    /// content dropped) makes `sendOneCore` fall back to the text/attachment
    /// path.
    pub top_level_nodes: usize,
}

impl ClipboardPayload {
    pub fn is_empty(&self) -> bool {
        self.top_level_nodes == 0
    }
}

/// `isInlineHtmlFormat`.
pub fn is_inline_html_format(format: &str) -> bool {
    let normalized = format.trim().to_ascii_lowercase();
    INLINE_HTML_FORMATS.contains(&normalized.as_str())
}

/// `createInvisibleBodyText`: a short random run of zero-width characters
/// (5–10 of them) that is invisible to the recipient but makes each outgoing
/// body byte-unique.
pub fn create_invisible_body_text() -> String {
    let span = ZERO_WIDTH_BODY_MAX_LENGTH - ZERO_WIDTH_BODY_MIN_LENGTH + 1;
    let length = ZERO_WIDTH_BODY_MIN_LENGTH + random_u32() % span;
    let mut text = String::new();
    for _ in 0..length {
        let index = (random_u32() as usize) % ZERO_WIDTH_BODY_CHARACTERS.len();
        text.push(ZERO_WIDTH_BODY_CHARACTERS[index]);
    }
    text
}

/// Append a fresh invisible body marker to a rich payload.
///
/// The same marker is pushed onto both flavours so the pasted HTML and its
/// plain-text fallback carry the identical invisible tail. It is wrapped in a
/// trailing `<span>` so the browser keeps the zero-width run out of the visible
/// text flow.
pub fn append_invisible_body_marker(payload: &mut ClipboardPayload) {
    let marker = create_invisible_body_text();
    payload.text.push_str(&marker);
    payload.html.push_str("<span>");
    payload.html.push_str(&marker);
    payload.html.push_str("</span>");
}

/// Entropy without a dedicated RNG crate: a v4 UUID is seeded from the OS CSPRNG.
fn random_u32() -> u32 {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// `safeEmailUrl`: keep `http(s)`, `mailto`, `tel` and fragment links, plus
/// base64 image data URLs for `src` attributes.
pub fn safe_email_url(value: &str, allow_data_image: bool) -> String {
    let url = value.trim();
    if REGEX_SCHEME
        .get_or_init(|| Regex::new(r"(?i)^(https?:|mailto:|tel:|#)").unwrap())
        .is_match(url)
    {
        return url.to_string();
    }
    if allow_data_image
        && REGEX_DATA_IMAGE
            .get_or_init(|| Regex::new(r"(?i)^data:image/(?:png|gif|jpe?g|webp);base64,").unwrap())
            .is_match(url)
    {
        return url.to_string();
    }
    String::new()
}

/// The three `style`-attribute scrubs of `parseSafeEmailHtml`.
fn sanitize_css(value: &str) -> String {
    let expression =
        REGEX_CSS_EXPRESSION.get_or_init(|| Regex::new(r"(?i)expression\s*\([^)]*\)").unwrap());
    let url_script = REGEX_CSS_URL_SCRIPT.get_or_init(|| {
        Regex::new(r#"(?i)url\s*\(\s*(['"]?)\s*(?:javascript|vbscript):[^)]*\)"#).unwrap()
    });
    let behavior = REGEX_CSS_BEHAVIOR.get_or_init(|| Regex::new(r"(?i)behavior\s*:[^;]*").unwrap());
    let stripped = expression.replace_all(value, "");
    let stripped = url_script.replace_all(&stripped, "");
    behavior.replace_all(&stripped, "").into_owned()
}

static REGEX_SCHEME: OnceLock<Regex> = OnceLock::new();
static REGEX_DATA_IMAGE: OnceLock<Regex> = OnceLock::new();
static REGEX_CSS_EXPRESSION: OnceLock<Regex> = OnceLock::new();
static REGEX_CSS_URL_SCRIPT: OnceLock<Regex> = OnceLock::new();
static REGEX_CSS_BEHAVIOR: OnceLock<Regex> = OnceLock::new();
static REGEX_NEWLINE_RUN: OnceLock<Regex> = OnceLock::new();

/// The mutable builder state. `Tokenizer::feed` takes `&self` on the tokenizer,
/// so the sink keeps its state behind a `RefCell`.
#[derive(Default)]
struct Inner {
    html: String,
    text: String,
    image_count: usize,
    /// Visited nodes outside any dropped subtree; the 20 000 cap mirrors
    /// `parseSafeEmailHtml`.
    node_count: usize,
    top_level_nodes: usize,
    /// Depth of currently emitting (sanitized) elements.
    depth: usize,
    /// How many nested tags deep we are inside a dropped (`script`…) subtree.
    suppressed: usize,
    /// Open elements, in order, so end tags can match and auto-close.
    open: Vec<Open>,
    /// Set when the node cap is exceeded so the caller can raise the exact
    /// desktop runtime error.
    too_large: bool,
}

struct Open {
    name: String,
    /// Whether this element was emitted (allowed) or unwrapped (unknown tag).
    emitted: bool,
}

/// The token sink handed to html5ever. A local newtype so the trait
/// implementation is legal (`impl TokenSink for &RefCell<Inner>` would trip the
/// orphan rule).
#[derive(Clone, Default)]
struct Sink {
    inner: Rc<RefCell<Inner>>,
}

impl Inner {
    fn escape_text(out: &mut String, value: &str) {
        for ch in value.chars() {
            match ch {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                _ => out.push(ch),
            }
        }
    }

    fn escape_attribute(value: &str) -> String {
        let mut out = String::with_capacity(value.len());
        for ch in value.chars() {
            match ch {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '"' => out.push_str("&quot;"),
                _ => out.push(ch),
            }
        }
        out
    }

    fn push_text(&mut self, value: &str) {
        if value.is_empty() {
            return;
        }
        if self.depth == 0 {
            self.top_level_nodes += 1;
        }
        Self::escape_text(&mut self.html, value);
        self.text.push_str(value);
    }

    fn start_tag(&mut self, name: &str, attrs: &[(String, String)], self_closing: bool) {
        if self.suppressed > 0 {
            // Inside a dropped subtree: track nesting only so the matching end
            // tag is skipped correctly. Void tags have no end tag.
            if !is_void(name) && !self_closing {
                self.suppressed += 1;
            }
            return;
        }

        self.node_count += 1;
        if self.node_count > 20_000 {
            self.too_large = true;
        }

        if is_drop_tag(name) {
            if !is_void(name) && !self_closing {
                self.suppressed = 1;
            }
            return;
        }

        let void = is_void(name) || self_closing;
        let allowed = EMAIL_HTML_TAGS.contains(&name);

        if !allowed {
            // Unknown tag: unwrap, keeping its children at the current depth.
            if !void {
                self.open.push(Open {
                    name: name.to_string(),
                    emitted: false,
                });
            }
            return;
        }

        // Emit an allowed element with its filtered attributes.
        if self.depth == 0 {
            self.top_level_nodes += 1;
        }
        self.html.push('<');
        self.html.push_str(name);
        for (attr_name, attr_value) in attrs {
            let lower = attr_name.to_ascii_lowercase();
            if lower.starts_with("on") || !EMAIL_HTML_ATTRIBUTES.contains(&lower.as_str()) {
                continue;
            }
            let value = if lower == "href" {
                safe_email_url(attr_value, false)
            } else if lower == "src" {
                safe_email_url(attr_value, name == "img")
            } else if lower == "style" {
                sanitize_css(attr_value)
            } else {
                attr_value.clone()
            };
            if (lower == "href" || lower == "src") && value.is_empty() {
                continue;
            }
            self.html.push(' ');
            self.html.push_str(&lower);
            self.html.push_str("=\"");
            self.html.push_str(&Self::escape_attribute(&value));
            self.html.push('"');
        }
        self.html.push('>');

        if name == "img" {
            self.image_count += 1;
        }
        if name == "br" {
            // `br` is a void element with no children, so the text flavour gets
            // its newline immediately.
            self.text.push('\n');
        }

        if void {
            return;
        }
        self.depth += 1;
        self.open.push(Open {
            name: name.to_string(),
            emitted: true,
        });
    }

    fn end_tag(&mut self, name: &str) {
        if self.suppressed > 0 {
            if !is_void(name) {
                self.suppressed -= 1;
            }
            return;
        }

        // Find the nearest matching open element; auto-close anything above it.
        let Some(position) = self.open.iter().rposition(|open| open.name == name) else {
            return;
        };
        while self.open.len() > position {
            let open = self.open.pop().expect("open element");
            if open.emitted {
                self.html.push_str("</");
                self.html.push_str(&open.name);
                self.html.push('>');
                if self.depth > 0 {
                    self.depth -= 1;
                }
                if TEXT_BREAK_TAGS.contains(&open.name.as_str()) {
                    self.text.push('\n');
                }
            }
        }
    }
}

impl TokenSink for Sink {
    type Handle = ();

    fn process_token(&self, token: Token, _line_number: u64) -> TokenSinkResult<()> {
        let mut builder = self.inner.borrow_mut();
        match token {
            CharacterTokens(value) => {
                if builder.suppressed == 0 {
                    builder.node_count += 1;
                    if builder.node_count > 20_000 {
                        builder.too_large = true;
                    }
                    builder.push_text(&value);
                }
            }
            NullCharacterToken => {
                if builder.suppressed == 0 {
                    builder.push_text(" ");
                }
            }
            TagToken(tag) => {
                let name = tag.name.as_ref().to_ascii_lowercase();
                match tag.kind {
                    TagKind::StartTag => {
                        let attrs: Vec<(String, String)> = tag
                            .attrs
                            .iter()
                            .map(|attr| (attr.name.local.to_string(), attr.value.to_string()))
                            .collect();
                        builder.start_tag(&name, &attrs, tag.self_closing);
                    }
                    TagKind::EndTag => builder.end_tag(&name),
                }
            }
            // Comments, doctype and parse errors carry no insertable content.
            _ => {}
        }
        TokenSinkResult::Continue
    }
}

fn is_void(name: &str) -> bool {
    VOID_TAGS.contains(&name)
}

fn is_drop_tag(name: &str) -> bool {
    EMAIL_HTML_DROP_TAGS.contains(&name)
}

/// `parseSafeEmailHtml` + `htmlTreeToClipboardPayload`: sanitize the inlined
/// email markup and render the clipboard payload. Errors when the tree exceeds
/// 20 000 nodes, with desktop runtime's exact message.
pub fn sanitize_email_html(html: &str) -> Result<ClipboardPayload, String> {
    let sink = Sink::default();
    let tokenizer = Tokenizer::new(sink.clone(), TokenizerOpts::default());

    let input = BufferQueue::default();
    input.push_back(StrTendril::from_slice(html));
    let _ = tokenizer.feed(&input);
    tokenizer.end();
    drop(tokenizer);

    let builder = sink.inner.borrow();

    if builder.too_large {
        return Err("The email HTML template is too large to insert.".to_string());
    }

    // `htmlTreeToClipboardPayload`'s final text normalisation.
    let collapsed = REGEX_NEWLINE_RUN
        .get_or_init(|| Regex::new(r"\n{3,}").unwrap())
        .replace_all(&builder.text, "\n\n");
    let text = collapsed.trim().to_string();

    Ok(ClipboardPayload {
        html: builder.html.clone(),
        text,
        image_count: builder.image_count,
        top_level_nodes: builder.top_level_nodes,
    })
}

/// A `", "`-joined selector list, mirroring the arrays desktop runtime passed into
/// `page.waitForFunction`. Kept as the one place that formatting lives for the script-building
/// helpers; the current call sites inline their selectors.
#[allow(dead_code)]
pub fn selector_list(selectors: &[&str]) -> String {
    selectors.join(", ")
}

/// The recipient (`To`) field selectors (`TO_FIELD_SELECTORS`).
pub const TO_FIELD_SELECTORS: &[&str] = &[
    "textarea[name=\"to\"]",
    "input[name=\"to\"]",
    "[aria-label=\"To recipients\" i]",
    "textarea[aria-label^=\"To\" i]",
    "input[aria-label^=\"To\" i]",
    "textarea[aria-label*=\"Recipients\" i]",
    "input[aria-label*=\"Recipients\" i]",
    "input[aria-label=\"Recipients\" i]",
    "[role=\"combobox\"][aria-label*=\"recipient\" i]",
    "[role=\"combobox\"][aria-label^=\"To\" i]",
    "input[role=\"combobox\"]",
    "textarea[role=\"combobox\"]",
    "[contenteditable=\"true\"][role=\"combobox\"]",
    "[role=\"textbox\"][aria-label*=\"recipient\" i]",
    "[aria-autocomplete=\"list\"][aria-label*=\"To\" i]",
    "input[placeholder=\"To\"]",
    "textarea[placeholder*=\"Recipients\" i]",
    "input[placeholder*=\"Recipients\" i]",
    "[contenteditable=\"true\"][aria-label*=\"To\" i]",
    "[contenteditable=\"true\"][aria-label*=\"Recipients\" i]",
    "[contenteditable=\"true\"][aria-autocomplete=\"list\"]",
    "input[peoplekit-id=\"BbVjBd\"]",
];

/// The subject field selectors (`SUBJECT_FIELD_SELECTORS`).
pub const SUBJECT_FIELD_SELECTORS: &[&str] = &[
    "input[name=\"subjectbox\"]",
    "input[placeholder=\"Subject\"]",
    "textarea[name=\"subject\"]",
];

/// The message body selectors (`COMPOSE_BODY_SELECTORS`).
///
/// Gmail localizes `aria-label`, so the English `"Message Body"` selectors miss
/// the editor on a non-English Gmail UI (which is common — a Bengali or German
/// Gmail fails the same way). Keep the English selectors first because they are
/// the most precise when they do match, then add markers that are stable across
/// UI languages: `g_editable="true"` and `aria-multiline="true"` are Gmail's own
/// editor attributes, and `Am`/`Al`/`editable` are the editor's long-lived class
/// list. The editor lives in the same place regardless of language, so these
/// match what the English `aria-label` was standing in for.
pub const COMPOSE_BODY_SELECTORS: &[&str] = &[
    "div[contenteditable=\"true\"][aria-label*=\"Message Body\"]",
    "div[contenteditable=\"true\"][role=\"textbox\"]",
    "div[role=\"textbox\"][aria-label*=\"Message Body\"]",
    "div[aria-label=\"Message Body\"]",
    "div[contenteditable=\"true\"][g_editable=\"true\"]",
    "div[g_editable=\"true\"]",
    "div[contenteditable=\"true\"][aria-multiline=\"true\"]",
    "div[contenteditable=\"true\"].editable",
];

/// Self-test for the pure (browser-free) part of the pipeline.
///
/// Run: `NOVAMAIL_SELFTEST_MODE=gmail`. Returns `{ checks: [...], passed: bool }`.
pub fn selftest() -> serde_json::Value {
    use serde_json::json;

    let mut checks: Vec<(String, bool, String)> = Vec::new();
    let mut check =
        |name: &str, ok: bool, detail: String| checks.push((name.to_string(), ok, detail));

    check(
        "inlineFormat",
        is_inline_html_format("inline")
            && is_inline_html_format(" HTML ")
            && !is_inline_html_format("pdf"),
        format!(
            "inline={} html={} pdf={}",
            is_inline_html_format("inline"),
            is_inline_html_format(" HTML "),
            is_inline_html_format("pdf")
        ),
    );

    let invisible = create_invisible_body_text();
    check(
        "invisibleBody",
        (5..=10).contains(&invisible.chars().count())
            && invisible
                .chars()
                .all(|ch| ZERO_WIDTH_BODY_CHARACTERS.contains(&ch)),
        format!("len={}", invisible.chars().count()),
    );

    check(
        "safeUrl",
        safe_email_url("https://example.com", false) == "https://example.com"
            && safe_email_url("javascript:alert(1)", false).is_empty()
            && safe_email_url("data:image/png;base64,AAA", true).starts_with("data:image/png")
            && safe_email_url("data:image/png;base64,AAA", false).is_empty(),
        "scheme allow-list".to_string(),
    );

    // Drop tags, unwrapped tags, attribute filtering, URL filtering and the
    // text flavour with break tags.
    let payload = sanitize_email_html(
        "<div class=\"x\"><script>alert(1)</script><span style=\"color:red\">Hi &amp; bye</span>\
         <a href=\"javascript:evil()\">bad</a><a href=\"https://ok.example\">good</a>\
         <img src=\"https://img.example/a.png\" onerror=\"x()\"><p>Line</p></div>",
    )
    .expect("sanitize");
    let html_ok = payload.html
        == "<div><span style=\"color:red\">Hi &amp; bye</span><a>bad</a><a href=\"https://ok.example\">good</a><img src=\"https://img.example/a.png\"><p>Line</p></div>";
    check("sanitizeHtml", html_ok, payload.html.clone());
    check(
        "sanitizeText",
        payload.text == "Hi & byebadgoodLine",
        format!("{:?}", payload.text),
    );
    // `p`/`br` contribute newlines to the text flavour.
    let breaks = sanitize_email_html("<p>A</p><p>B</p><br>C").expect("sanitize");
    check(
        "textBreaks",
        breaks.text == "A\nB\n\nC",
        format!("{:?}", breaks.text),
    );
    check(
        "sanitizeImages",
        payload.image_count == 1,
        format!("{}", payload.image_count),
    );
    check(
        "sanitizeNotEmpty",
        !payload.is_empty(),
        format!("{}", payload.top_level_nodes),
    );

    // All content dropped -> empty tree (falls back to the text/attachment path).
    let dropped = sanitize_email_html("<script>x</script>").expect("sanitize");
    check(
        "dropAll",
        dropped.is_empty(),
        format!("{}", dropped.top_level_nodes),
    );

    // The exact oversize error.
    let mut huge = String::new();
    for _ in 0..20_001 {
        huge.push_str("<br>");
    }
    let oversize = sanitize_email_html(&huge);
    check(
        "oversize",
        oversize.as_ref().err().map(String::as_str)
            == Some("The email HTML template is too large to insert."),
        format!("{oversize:?}"),
    );

    // Gmail localizes `aria-label`, so the body selectors must include at least
    // one marker that survives a non-English UI (Gmail's own editor attributes),
    // otherwise compose is never recognized as open. Guard against a regression
    // back to English-only selectors.
    let language_independent = COMPOSE_BODY_SELECTORS
        .iter()
        .any(|selector| selector.contains("g_editable") || selector.contains("aria-multiline"));
    check(
        "bodySelectorsLocalized",
        language_independent,
        COMPOSE_BODY_SELECTORS.join(" | "),
    );

    let passed = checks.iter().all(|(_, ok, _)| *ok);
    let checks: Vec<serde_json::Value> = checks
        .into_iter()
        .map(|(name, ok, detail)| json!({ "name": name, "passed": ok, "detail": detail }))
        .collect();
    json!({ "checks": checks, "passed": passed })
}
