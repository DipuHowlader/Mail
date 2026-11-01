//! Native file pickers and import parsing for an app window.
//!
//! Ports the `recipients:*` / `templates:*` / `subjects:*` / `attachments:*`
//! channels from `desktop-runtime/appRuntime.cjs`. The renderer never touches the
//! filesystem: it asks for a *parsed* result and gets plain data back, which is
//! the same contract as before.
//!
//! The dialog itself comes from `rfd` (the OS-native open dialog) rather than
//! the Tauri dialog plugin, so nothing has to be exposed to the webview and no
//! capability entry is needed — the webview still only ever calls our commands.

use std::path::Path;

use calamine::{Data, Reader, open_workbook_auto};
use serde_json::{Map, Value, json};

/// Mirrors `100_000` in `appRuntime.cjs`.
const MAX_SUBJECT_BYTES: u64 = 100_000;
/// Mirrors `2_000_000` in `appRuntime.cjs`.
const MAX_ATTACHMENT_BYTES: u64 = 2_000_000;

/// One imported row: ordered header → value pairs. Insertion order is
/// preserved so email-column detection behaves exactly like the JS version
/// (which relies on `Object.keys` / `Object.values` order).
type Row = Vec<(String, String)>;

// --- Validation (ported from appRuntime.cjs) -----------------------------------

/// Equivalent of `/^[^\s@]+@[^\s@]+\.[^\s@]+$/`.
fn is_valid_email(value: &str) -> bool {
    let value = value.trim();
    let mut parts = value.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let is_chunk =
        |text: &str| !text.is_empty() && !text.chars().any(|ch| ch == '@' || ch.is_whitespace());
    let dotted = match domain.rfind('.') {
        Some(index) => index > 0 && index + 1 < domain.len(),
        None => false,
    };
    is_chunk(local) && is_chunk(domain) && dotted
}

/// Equivalent of `/^e[-\s]?mail/i` applied to a trimmed header.
fn looks_like_email_header(header: &str) -> bool {
    let mut chars = header.trim().chars();
    if !matches!(chars.next(), Some('e') | Some('E')) {
        return false;
    }
    let rest: String = chars.collect();
    if rest.to_ascii_lowercase().starts_with("mail") {
        return true;
    }
    let mut rest = rest.chars();
    match rest.next() {
        Some(separator) if separator == '-' || separator.is_whitespace() => {
            rest.as_str().to_ascii_lowercase().starts_with("mail")
        }
        _ => false,
    }
}

fn row_get<'a>(row: &'a Row, key: &str) -> Option<&'a str> {
    row.iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
}

/// JS `a || b || c` over row fields: the first non-empty value wins.
fn pick_value(row: &Row, keys: &[&str]) -> String {
    for key in keys {
        if let Some(value) = row_get(row, key) {
            if !value.is_empty() {
                return value.to_string();
            }
        }
    }
    String::new()
}

fn read_text(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("Could not read the file: {error}"))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn format_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        (value as i64).to_string()
    } else {
        value.to_string()
    }
}

fn cell_to_string(cell: &Data) -> String {
    match cell {
        Data::Empty => String::new(),
        Data::String(value) => value.clone(),
        Data::Int(value) => value.to_string(),
        Data::Float(value) => format_number(*value),
        Data::Bool(value) => value.to_string(),
        Data::DateTimeIso(value) | Data::DurationIso(value) => value.clone(),
        Data::DateTime(value) => format_number(value.as_f64()),
        Data::Error(value) => format!("{value:?}"),
    }
}

/// `XLSX.utils.sheet_to_json` names duplicate headers `a`, `a_1`, … and blank
/// headers `__EMPTY`, `__EMPTY_1`, ….
fn dedupe_headers(headers: Vec<String>) -> Vec<String> {
    let mut seen: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    headers
        .into_iter()
        .map(|header| {
            let base = if header.is_empty() {
                "__EMPTY".to_string()
            } else {
                header
            };
            let count = seen.entry(base.clone()).or_insert(0);
            let resolved = if *count == 0 {
                base.clone()
            } else {
                format!("{base}_{count}")
            };
            *count += 1;
            resolved
        })
        .collect()
}

// --- Readers -------------------------------------------------------------------

fn keep_row(row: &Row) -> bool {
    row.iter().any(|(_, value)| !value.trim().is_empty())
}

fn read_csv_rows(path: &Path) -> Result<Vec<Row>, String> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .from_path(path)
        .map_err(|error| format!("Could not read the file: {error}"))?;

    let headers: Vec<String> = reader
        .headers()
        .map_err(|error| format!("Could not read the file: {error}"))?
        .iter()
        .map(str::to_string)
        .collect();

    let mut rows = Vec::new();
    for record in reader.records() {
        let record = record.map_err(|error| format!("Could not read the file: {error}"))?;
        let row: Row = headers
            .iter()
            .enumerate()
            .map(|(index, header)| (header.clone(), record.get(index).unwrap_or("").to_string()))
            .collect();
        if keep_row(&row) {
            rows.push(row);
        }
    }
    Ok(rows)
}

fn read_sheet_rows(path: &Path) -> Result<Vec<Row>, String> {
    let mut workbook = open_workbook_auto(path)
        .map_err(|error| format!("Could not read the spreadsheet: {error}"))?;
    let sheet = workbook
        .sheet_names()
        .first()
        .cloned()
        .ok_or_else(|| "The spreadsheet has no sheets.".to_string())?;
    let range = workbook
        .worksheet_range(&sheet)
        .map_err(|error| format!("Could not read the spreadsheet: {error}"))?;

    let mut iterator = range.rows();
    let Some(header_row) = iterator.next() else {
        return Ok(Vec::new());
    };
    let headers = dedupe_headers(header_row.iter().map(cell_to_string).collect());

    let mut rows = Vec::new();
    for cells in iterator {
        let row: Row = headers
            .iter()
            .enumerate()
            .map(|(index, header)| {
                (
                    header.clone(),
                    cells.get(index).map(cell_to_string).unwrap_or_default(),
                )
            })
            .collect();
        if keep_row(&row) {
            rows.push(row);
        }
    }
    Ok(rows)
}

/// Read a recipient file (CSV / XLSX / XLS) into ordered rows.
pub fn read_rows(path: &Path) -> Result<Vec<Row>, String> {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match extension.as_str() {
        "csv" => read_csv_rows(path),
        "xlsx" | "xls" => read_sheet_rows(path),
        "" => Err("The file has no extension.".to_string()),
        other => Err(format!("Unsupported file type: .{other}")),
    }
}

// --- Recipient extraction (ported from `extractEmailColumn`) -------------------

/// `String(key).trim().toLowerCase().replace(/[^a-z0-9_]+/g,'_').replace(/^_+|_+$/g,'')`
fn normalize_key(key: &str) -> String {
    let lowered = key.trim().to_ascii_lowercase();
    let mut out = String::with_capacity(lowered.len());
    let mut pending = false;
    for ch in lowered.chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' {
            if pending {
                out.push('_');
            }
            pending = false;
            out.push(ch);
        } else {
            pending = true;
        }
    }
    out.trim_matches('_').to_string()
}

fn custom_fields(row: &Row) -> Map<String, Value> {
    let mut map = Map::new();
    for (raw_key, raw_value) in row {
        let key = normalize_key(raw_key);
        let mut chars = key.chars();
        let shape_ok = matches!(chars.next(), Some('a'..='z') | Some('_'))
            && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_');
        if !shape_ok {
            continue;
        }
        if matches!(key.as_str(), "__proto__" | "prototype" | "constructor") {
            continue;
        }
        map.insert(key, json!(raw_value.trim()));
    }
    map
}

/// `extractEmailColumn(rows)`: pick an email column, de-duplicate, and flatten
/// each row into `{ ...customFields, email, name, order_id, amount }`.
pub fn recipients_from_rows(rows: &[Row]) -> Vec<Value> {
    let Some(first) = rows.first() else {
        return Vec::new();
    };
    let headers: Vec<&str> = first.iter().map(|(key, _)| key.as_str()).collect();

    let email_key = headers
        .iter()
        .find(|key| looks_like_email_header(key))
        .copied()
        // Faithful oddity: the original also accepts a *header* that is itself
        // a valid email address.
        .or_else(|| headers.iter().find(|key| is_valid_email(key)).copied());

    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();

    for row in rows {
        let candidates: Vec<&str> = match email_key {
            Some(key) => row_get(row, key).into_iter().collect(),
            None => row.iter().map(|(_, value)| value.as_str()).collect(),
        };
        let Some(email) = candidates
            .into_iter()
            .find(|value| is_valid_email(value))
            .map(|value| value.trim().to_string())
        else {
            continue;
        };
        if !seen.insert(email.to_ascii_lowercase()) {
            continue;
        }

        let mut record = custom_fields(row);
        record.insert("email".into(), json!(email));
        record.insert(
            "name".into(),
            json!(pick_value(row, &["name", "Name", "full_name", "Full Name"])),
        );
        record.insert(
            "order_id".into(),
            json!(pick_value(row, &["order_id", "orderId", "Order ID"])),
        );
        record.insert(
            "amount".into(),
            json!(pick_value(row, &["amount", "Amount", "total", "Total"])),
        );
        out.push(Value::Object(record));
    }

    out
}

/// Read + parse a recipient file. Returns `(totalRows, recipients)`.
pub fn parse_recipient_file(path: &Path) -> Result<(usize, Vec<Value>), String> {
    let rows = read_rows(path)?;
    let total = rows.len();
    Ok((total, recipients_from_rows(&rows)))
}

// --- Commands ------------------------------------------------------------------
// Mirror the recipients:* / templates:* / subjects:* / attachments:* channels.

#[tauri::command]
pub async fn recipients_pick_and_parse() -> Result<Value, String> {
    let Some(handle) = rfd::AsyncFileDialog::new()
        .set_title("Import recipients")
        .add_filter("Recipient list", &["csv", "xlsx", "xls"])
        .pick_file()
        .await
    else {
        return Ok(Value::Null);
    };

    let (total, recipients) = parse_recipient_file(handle.path())?;
    Ok(json!({
        "fileName": handle.file_name(),
        "totalRows": total,
        "recipients": recipients,
    }))
}

#[tauri::command]
pub async fn templates_pick() -> Result<Value, String> {
    let Some(handle) = rfd::AsyncFileDialog::new()
        .set_title("Choose HTML email template")
        .add_filter("HTML template", &["html", "htm"])
        .pick_file()
        .await
    else {
        return Ok(Value::Null);
    };

    Ok(json!({
        "fileName": handle.file_name(),
        "html": read_text(handle.path())?,
    }))
}

#[tauri::command]
pub async fn subjects_pick() -> Result<Value, String> {
    let Some(handle) = rfd::AsyncFileDialog::new()
        .set_title("Choose subject template")
        .add_filter("Text file", &["txt", "md"])
        .pick_file()
        .await
    else {
        return Ok(Value::Null);
    };

    let path = handle.path();
    let size = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    if size > MAX_SUBJECT_BYTES {
        return Err("Subject template files must be 100 KB or smaller.".to_string());
    }

    let content = read_text(path)?;
    let subject_template = content
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_string();

    Ok(json!({
        "fileName": handle.file_name(),
        "subjectTemplate": subject_template,
    }))
}

#[tauri::command]
pub async fn attachments_pick() -> Result<Value, String> {
    let Some(handle) = rfd::AsyncFileDialog::new()
        .set_title("Attach HTML template")
        .add_filter("HTML template", &["html", "htm"])
        .pick_file()
        .await
    else {
        return Ok(Value::Array(Vec::new()));
    };

    let path = handle.path();
    let filename = handle.file_name();
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !matches!(extension.as_str(), "html" | "htm") {
        return Err("Only HTML files can be selected here.".to_string());
    }

    let size = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    if size > MAX_ATTACHMENT_BYTES {
        return Err("HTML template files must be 2 MB or smaller.".to_string());
    }

    Ok(json!([{
        "filename": filename,
        "path": path.display().to_string(),
        "size": size,
        "tooLarge": false,
        "templateContent": read_text(path)?,
    }]))
}
