//! Tauri command surface.
//!
//! Command names mirror the desktop runtime IPC channels one-for-one (see
//! `desktop-runtime/preload.cjs`); the frontend keeps talking to the same `window.api.*`
//! methods because `src/services/tauriBridge.js` maps them here.
//!
//! Phase 1 status:
//!   - `window:*`      implemented
//!   - `license:*`     implemented (real service verification)
//!   - `campaigns:*`   implemented (shared config store)
//!   - `launcher:*`    read-only stubs (registry + process spawning land next)
//!   - everything else rejects until its phase.

use std::cmp::Reverse;
use std::sync::Mutex;

use serde_json::{Value, json};
use tauri::{Emitter, State, WebviewWindow};

use crate::license;
use crate::store::{Store, now_ms};

// --- Window chrome (mirrors window:* channels) ---------------------------------

#[tauri::command]
pub fn window_minimize(window: WebviewWindow) {
    let _ = window.minimize();
}

#[tauri::command]
pub fn window_maximize(window: WebviewWindow) {
    if window.is_maximized().unwrap_or(false) {
        let _ = window.unmaximize();
    } else {
        let _ = window.maximize();
    }
    // Tell the renderer so its maximize/restore icon tracks the window. The
    // OS also reports this through the `Resized` handler in `runtime.rs`, which
    // covers the double-click-titlebar case, but emitting here keeps the
    // response immediate.
    let _ = window.emit(
        "window:state",
        json!({ "maximized": window.is_maximized().unwrap_or(false) }),
    );
}

#[tauri::command]
pub fn window_close(window: WebviewWindow) {
    let _ = window.close();
}

#[tauri::command]
pub fn window_is_maximized(window: WebviewWindow) -> bool {
    window.is_maximized().unwrap_or(false)
}

// --- License (mirrors license:* channels) --------------------------------------

/// Re-verify the stored key on every start, exactly like desktop runtime: a license can
/// be revoked or expire between runs, so the gate must not trust local state.
#[tauri::command]
pub async fn license_check_stored(store: State<'_, Mutex<Store>>) -> Result<Value, String> {
    let key = {
        let mut guard = store.lock().map_err(|error| error.to_string())?;
        guard.refresh()?;
        license::read_stored_key(&guard)
    };
    let Some(key) = key else {
        return Ok(json!({ "valid": false, "code": "NO_STORED_LICENSE" }));
    };

    match license::verify_remote(&key).await {
        Ok(result) => {
            if result.valid {
                if let Ok(mut guard) = store.lock() {
                    let _ = license::save(&mut guard, &key, &result);
                }
            }
            serde_json::to_value(result).map_err(|error| error.to_string())
        }
        Err(code) => Ok(json!({ "valid": false, "code": code })),
    }
}

#[tauri::command]
pub async fn license_verify(key: String, store: State<'_, Mutex<Store>>) -> Result<Value, String> {
    if key.trim().is_empty() {
        return Ok(json!({ "valid": false, "code": "LICENSE_KEY_REQUIRED" }));
    }

    match license::verify_remote(&key).await {
        Ok(result) => {
            if result.valid {
                if let Ok(mut guard) = store.lock() {
                    let _ = license::save(&mut guard, &key, &result);
                }
            }
            serde_json::to_value(result).map_err(|error| error.to_string())
        }
        Err(code) => Ok(json!({ "valid": false, "code": code })),
    }
}

// --- Campaigns (mirrors campaigns:* channels) ----------------------------------
//
// Saved campaigns live in the shared config store so the desktop runtime and Tauri
// builds see the same list during the migration.

#[tauri::command]
pub fn campaigns_list(store: State<'_, Mutex<Store>>) -> Vec<Value> {
    let Ok(mut guard) = store.lock() else {
        return Vec::new();
    };
    if let Err(error) = guard.refresh() {
        log::warn!("Could not refresh campaigns from the shared store: {error}");
    }
    let mut campaigns = array_at(&guard, "campaigns");
    campaigns
        .sort_by_key(|item| Reverse(item.get("updatedAt").and_then(Value::as_i64).unwrap_or(0)));
    campaigns
}

#[tauri::command]
pub fn campaigns_save(campaign: Value, store: State<'_, Mutex<Store>>) -> Result<Value, String> {
    if !campaign.is_object() {
        return Err("Campaign data is required.".to_string());
    }
    let record = normalize_campaign(&campaign);
    let id = record
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let mut guard = store.lock().map_err(|error| error.to_string())?;
    guard.transact(|data| {
        let mut campaigns = data
            .get("campaigns")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if let Some(existing) = campaigns
            .iter_mut()
            .find(|item| item.get("id").and_then(Value::as_str) == Some(id.as_str()))
        {
            *existing = record.clone();
        } else {
            campaigns.insert(0, record.clone());
        }
        data.insert("campaigns".to_string(), Value::Array(campaigns));
        Ok(record)
    })
}

#[tauri::command]
pub fn campaigns_delete(
    campaign_id: String,
    store: State<'_, Mutex<Store>>,
) -> Result<Vec<Value>, String> {
    if campaign_id.trim().is_empty() {
        return Err("Campaign id is required.".to_string());
    }
    let mut guard = store.lock().map_err(|error| error.to_string())?;
    guard.transact(|data| {
        let campaigns: Vec<Value> = data
            .get("campaigns")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|item| item.get("id").and_then(Value::as_str) != Some(campaign_id.as_str()))
            .collect();
        data.insert("campaigns".to_string(), Value::Array(campaigns.clone()));
        Ok(campaigns)
    })
}

fn array_at(store: &Store, key: &str) -> Vec<Value> {
    store
        .get(key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn is_valid_email(value: &str) -> bool {
    let value = value.trim();
    let mut parts = value.split('@');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(local), Some(domain), None) => {
            !local.is_empty()
                && !local.contains(char::is_whitespace)
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
                && !domain.contains(char::is_whitespace)
        }
        _ => false,
    }
}

/// Same field normalization as `campaigns:save` in `desktop-runtime/appRuntime.cjs`, so
/// both builds write identical records into the shared store.
fn normalize_campaign(campaign: &Value) -> Value {
    let get_str = |key: &str| {
        campaign
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let get_array = |key: &str| {
        campaign
            .get(key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };

    let raw_id = get_str("id");
    let id = if raw_id.trim().is_empty() {
        uuid::Uuid::new_v4().to_string()
    } else {
        raw_id
    };

    let subject = get_str("subject");
    let subject_template = if campaign
        .get("subjectTemplate")
        .and_then(Value::as_str)
        .is_some()
    {
        get_str("subjectTemplate")
    } else {
        subject.clone()
    };

    let instance_settings = campaign
        .get("instanceSettings")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(normalize_instance_setting)
        .collect::<Vec<_>>();

    json!({
        "id": id,
        // Campaigns are account-agnostic: Gmail sessions belong to automation
        // workers and are supplied only for the duration of a send run.
        "accountId": null,
        "accountEmail": null,
        "to": get_array("to"),
        "recipientData": get_array("recipientData"),
        "cc": get_array("cc"),
        "bcc": get_array("bcc"),
        "subject": subject,
        "subjectTemplate": subject_template,
        "bodyHtml": get_str("bodyHtml"),
        "rawHtmlTemplate": get_str("rawHtmlTemplate"),
        "attachments": get_array("attachments"),
        "baseAmount": get_str("baseAmount"),
        "tfn": get_str("tfn"),
        "companyName": get_str("companyName"),
        "outputFormat": if campaign.get("outputFormat").and_then(Value::as_str).is_some() {
            get_str("outputFormat")
        } else {
            "inline-text".to_string()
        },
        "autoGenerateFile": campaign.get("autoGenerateFile").and_then(Value::as_bool).unwrap_or(false),
        "fileNameTemplate": if campaign.get("fileNameTemplate").and_then(Value::as_str).is_some() {
            get_str("fileNameTemplate")
        } else {
            "Invoice_$invoice_no_$username_$customer_id".to_string()
        },
        "allocationLimits": campaign
            .get("allocationLimits")
            .filter(|value| value.is_object())
            .cloned()
            .unwrap_or_else(|| json!({})),
        "instanceSettings": instance_settings,
        "createdAt": campaign.get("createdAt").and_then(Value::as_i64).unwrap_or_else(now_ms),
        "updatedAt": now_ms(),
    })
}

fn normalize_instance_setting(item: &Value) -> Value {
    let raw_id = item
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let id = if raw_id.trim().is_empty() {
        uuid::Uuid::new_v4().to_string()
    } else {
        raw_id
    };

    let limit = match item.get("limit") {
        Some(Value::String(text)) if text.is_empty() => json!(""),
        Some(value) => {
            let number = value
                .as_f64()
                .or_else(|| value.as_str().and_then(|text| text.parse::<f64>().ok()))
                .unwrap_or(0.0);
            json!(number.max(0.0))
        }
        None => json!(0),
    };

    let account_id = item
        .get("accountId")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(|value| json!(value))
        .unwrap_or(Value::Null);

    let recipients: Vec<Value> = item
        .get("recipients")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|recipient| {
            let email = recipient
                .get("email")
                .and_then(Value::as_str)?
                .trim()
                .to_string();
            if !is_valid_email(&email) {
                return None;
            }
            let mut object = recipient.as_object().cloned().unwrap_or_default();
            object.insert("email".into(), json!(email));
            Some(Value::Object(object))
        })
        .collect();

    json!({ "id": id, "limit": limit, "accountId": account_id, "recipients": recipients })
}

// --- Launcher -----------------------------------------------------------------
//
// The real registry + process supervisor lives in `crate::launcher`; its
// commands are registered directly in `lib.rs`.

// --- Automation (mirrors automation:* channels) --------------------------------
//
// Not stubs: the real queue/telemetry pipeline lives in `crate::automation` and
// registers its commands from `lib.rs`.

// --- Documents (mirrors document:convert) --------------------------------------
//
// Implemented natively in `src/documents.rs` (Phase 3) and registered from
// `lib.rs`; it needs the managed `BrowserService`, so it does not live here.
