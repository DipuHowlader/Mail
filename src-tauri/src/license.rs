//! License verification — the Rust port of `electron/license.cjs`.
//!
//! The license is activated once, in the launcher (the dashboard). The key is
//! stored in the shared config store so an app process never asks for it again.
//!
//! Two things must match the desktop runtime build exactly:
//!
//! 1. **The device id.** The license service binds a key to a machine, so the hash
//!    we send must be derived from the same inputs: the OS `MachineGuid` combined
//!    with the app salt using SHA-256. See [`machine_id`].
//! 2. **The request/response contract.** Same URL, same JSON body, and the same
//!    `{ valid, code, tier, expiresAt }` result the gate understands.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::store::{Store, now_ms};

const DEFAULT_LICENSE_SERVER_URL: &str = "https://nova-main-backend.vercel.app/api/verify-license";

/// Whole-request budget, matching desktop runtime's `AbortSignal.timeout(12_000)`.
const REQUEST_TIMEOUT: Duration = Duration::from_millis(12_000);

/// Per-connection budget.
///
/// The license host answers with several A records and any one of them can be
/// unreachable. Our HTTP stack tries the resolved addresses in order, and
/// without a connect timeout the first dead address consumes the whole request
/// budget: verification then fails even though a healthy address was available.
/// Chromium hides this by racing addresses; a connect timeout makes hyper move
/// on to the next one. hyper divides this value across the resolved addresses.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(6);

/// desktop runtime retries the transport once before giving up, so we do too.
const TRANSPORT_ATTEMPTS: usize = 2;

/// Top-level key in the shared config store.
///
/// Deliberately separate from desktop runtime's `license` key. desktop runtime encrypts with its
/// own `safeStorage` scheme, which we cannot read back (verified empirically), and
/// writing our format into `license` would break the desktop runtime build still shipping
/// during the migration. Phase 7 merges the two once desktop runtime is removed.
const STORE_KEY: &str = "licenseNative";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LicenseResult {
    pub valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    /// Short, human-readable explanation supplied by the service (for example
    /// why a key was refused). desktop runtime surfaced this too, and the gate uses it
    /// when it has no better copy of its own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl LicenseResult {
    fn rejected(code: &str) -> Self {
        Self {
            valid: false,
            code: Some(code.to_string()),
            tier: None,
            expires_at: None,
            reason: None,
        }
    }
}

/// Server messages are shown to the user, so keep them short and printable.
fn sanitize_reason(value: Option<&Value>) -> Option<String> {
    let text = value?.as_str()?.trim();
    if text.is_empty() || text.len() > 200 {
        return None;
    }
    Some(text.to_string())
}

pub fn server_url() -> String {
    std::env::var("LICENSE_SERVER_URL").unwrap_or_else(|_| DEFAULT_LICENSE_SERVER_URL.to_string())
}

fn platform_label() -> &'static str {
    if cfg!(target_os = "windows") {
        "Windows"
    } else if cfg!(target_os = "macos") {
        "macOS"
    } else if cfg!(target_os = "linux") {
        "Linux"
    } else {
        "Unknown"
    }
}

/// SHA-256(`postbox-command|<raw>`) — identical to `electron/license.cjs`, so the
/// same machine resolves to the same device id on the license server.
fn hash_machine_id(raw: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("postbox-command|{raw}"));
    hex::encode(hasher.finalize())
}

/// Match `node-machine-id`'s `original: true` normalization before hashing.
///
/// The Windows registry value is normally already lowercase, but the registry
/// does not guarantee that. Hashing the value verbatim would make the Tauri
/// device id differ from desktop runtime's on machines whose `MachineGuid` uses
/// uppercase characters or contains formatting whitespace.
fn canonical_machine_id(raw: &str) -> String {
    raw.chars()
        .filter(|character| !character.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

#[cfg(windows)]
fn raw_machine_id() -> Result<String, String> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ};

    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let key = hklm
        .open_subkey_with_flags(r"SOFTWARE\Microsoft\Cryptography", KEY_READ)
        .map_err(|error| error.to_string())?;
    key.get_value::<String, _>("MachineGuid")
        .map(|value| canonical_machine_id(&value))
        .map_err(|error| error.to_string())
}

#[cfg(not(windows))]
fn raw_machine_id() -> Result<String, String> {
    // Keep the same source precedence as node-machine-id on Linux.
    for path in ["/var/lib/dbus/machine-id", "/etc/machine-id"] {
        if let Ok(value) = std::fs::read_to_string(path) {
            let canonical = canonical_machine_id(&value);
            if !canonical.is_empty() {
                return Ok(canonical);
            }
        }
    }
    Err("no machine id available".to_string())
}

/// The stable, one-way device identifier sent with every verification request.
pub fn machine_id() -> Result<String, String> {
    Ok(hash_machine_id(&raw_machine_id()?))
}

#[cfg(test)]
mod tests {
    use super::{canonical_machine_id, hash_machine_id};

    #[test]
    fn canonicalizes_machine_id_like_electron() {
        assert_eq!(canonical_machine_id("  ABCD-1234\r\n"), "abcd-1234");
    }

    #[test]
    fn hashes_the_canonical_machine_id() {
        assert_eq!(
            hash_machine_id(&canonical_machine_id(" ABCD-1234 ")),
            hash_machine_id("abcd-1234")
        );
    }
}

fn is_valid_code(code: &str) -> bool {
    !code.is_empty()
        && code.len() <= 64
        && code
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

/// Contact the license service. Transport failures come back as stable error
/// codes (never as raw network errors) so the UI can message the user sensibly.
pub async fn verify_remote(key: &str) -> Result<LicenseResult, String> {
    let url = server_url();
    let machine = machine_id().map_err(|_| "DEVICE_ID_UNAVAILABLE".to_string())?;

    let body = json!({
        "licenseKey": key.trim().to_uppercase(),
        "machineId": machine,
        "appVersion": env!("CARGO_PKG_VERSION"),
        "clientPlatform": platform_label(),
    });

    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .map_err(|_| "LICENSE_SERVICE_UNAVAILABLE".to_string())?;

    let mut last_error: Option<reqwest::Error> = None;
    for attempt in 0..TRANSPORT_ATTEMPTS {
        // The pause before a retry gives a different address a chance to win the
        // next lookup, mirroring desktop runtime's retry-then-fall-back behaviour.
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }

        match client.post(&url).json(&body).send().await {
            Ok(response) => return Ok(interpret(response).await),
            Err(error) => {
                if std::env::var("NOVAMAIL_LICENSE_DEBUG").as_deref() == Ok("1") {
                    eprintln!(
                        "license: transport attempt {} failed: {error:?}",
                        attempt + 1
                    );
                }
                // Windows caches DNS answers globally. If Vercel has rotated
                // its edge addresses and the cached pair is unreachable on
                // this network, a normal retry can receive that same pair and
                // fail identically. Clear the local resolver cache before the
                // retry so the OS can ask DNS for a fresh answer.
                if attempt + 1 < TRANSPORT_ATTEMPTS && (error.is_connect() || error.is_timeout()) {
                    let refreshed = refresh_system_dns_cache();
                    if std::env::var("NOVAMAIL_LICENSE_DEBUG").as_deref() == Ok("1") {
                        eprintln!("license: refreshed system DNS cache before retry: {refreshed}");
                    }
                }
                last_error = Some(error);
            }
        }
    }

    let error = last_error.expect("at least one transport attempt");
    Err(if error.is_timeout() {
        "LICENSE_SERVER_TIMEOUT".to_string()
    } else {
        "LICENSE_SERVER_UNREACHABLE".to_string()
    })
}

#[cfg(windows)]
fn refresh_system_dns_cache() -> bool {
    // DnsFlushResolverCache is exported by dnsapi.dll on supported Windows
    // versions. It only clears the local DNS client cache; it does not change
    // DNS settings or contact a remote service.
    #[link(name = "dnsapi")]
    unsafe extern "system" {
        fn DnsFlushResolverCache() -> i32;
    }

    // SAFETY: this is a parameterless Windows DNS client API.
    unsafe { DnsFlushResolverCache() != 0 }
}

#[cfg(not(windows))]
fn refresh_system_dns_cache() -> bool {
    false
}

/// Turn a license service response into a [`LicenseResult`].
///
/// The service signals rejection with a non-2xx status plus a machine-readable
/// `code`, so both shapes end up as a result the gate can message; transport
/// failures never reach here.
async fn interpret(response: reqwest::Response) -> LicenseResult {
    let status = response.status();
    let text = response.text().await.unwrap_or_default();

    if status.as_u16() >= 500 || status.as_u16() == 429 {
        return LicenseResult::rejected("LICENSE_SERVICE_UNAVAILABLE");
    }

    let data: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let code = data
        .get("code")
        .and_then(Value::as_str)
        .filter(|value| is_valid_code(value))
        .map(|value| value.to_uppercase());

    if !status.is_success() {
        let mut result = LicenseResult::rejected(code.as_deref().unwrap_or("LICENSE_REJECTED"));
        result.reason = sanitize_reason(data.get("reason"));
        return result;
    }

    LicenseResult {
        valid: data.get("valid").and_then(Value::as_bool).unwrap_or(false),
        code,
        tier: data.get("tier").and_then(Value::as_str).map(str::to_string),
        expires_at: data.get("expiresAt").and_then(Value::as_i64),
        reason: sanitize_reason(data.get("reason")),
    }
}

// --- Protected key storage -----------------------------------------------------

/// Read the stored activation key, if one exists and can be decrypted.
pub fn read_stored_key(store: &Store) -> Option<String> {
    let entry = store.get(STORE_KEY)?;
    match entry.get("scheme").and_then(Value::as_str).unwrap_or("") {
        "dpapi" => unprotect(entry.get("keyProtected")?.as_str()?),
        "plain" => entry.get("key").and_then(Value::as_str).map(str::to_string),
        _ => None,
    }
}

/// Persist a freshly verified key. Uses Windows DPAPI where available and falls
/// back to plaintext (marked as such) on other platforms.
pub fn save(store: &mut Store, key: &str, result: &LicenseResult) -> Result<(), String> {
    let normalized = key.trim().to_uppercase();
    let entry = match protect(&normalized) {
        Some(blob) => json!({
            "scheme": "dpapi",
            "keyProtected": blob,
            "key": null,
            "verifiedAt": now_ms(),
            "expiresAt": result.expires_at,
            "tier": result.tier,
        }),
        None => json!({
            "scheme": "plain",
            "keyProtected": null,
            "key": normalized,
            "verifiedAt": now_ms(),
            "expiresAt": result.expires_at,
            "tier": result.tier,
        }),
    };
    store.set(STORE_KEY, entry)
}

fn protect(key: &str) -> Option<String> {
    #[cfg(windows)]
    {
        use base64::Engine as _;
        let blob = dpapi::process(key.as_bytes(), true)?;
        Some(base64::engine::general_purpose::STANDARD.encode(blob))
    }
    #[cfg(not(windows))]
    {
        let _ = key;
        None
    }
}

fn unprotect(encoded: &str) -> Option<String> {
    #[cfg(windows)]
    {
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .ok()?;
        let plain = dpapi::process(&bytes, false)?;
        String::from_utf8(plain).ok()
    }
    #[cfg(not(windows))]
    {
        let _ = encoded;
        None
    }
}

// --- Windows DPAPI (raw FFI: no extra crate, no extra link dependency) ----------

#[cfg(windows)]
mod dpapi {
    use std::ffi::c_void;
    use std::ptr;

    #[repr(C)]
    struct DataBlob {
        cb_data: u32,
        pb_data: *mut u8,
    }

    // SAFETY: these are the documented DPAPI entry points exported by crypt32.dll.
    #[link(name = "crypt32")]
    unsafe extern "system" {
        fn CryptProtectData(
            p_data_in: *const DataBlob,
            sz_description: *const u16,
            p_optional_entropy: *const DataBlob,
            pv_reserved: *const c_void,
            p_prompt_struct: *const c_void,
            dw_flags: u32,
            p_data_out: *mut DataBlob,
        ) -> i32;
        fn CryptUnprotectData(
            p_data_in: *const DataBlob,
            ppsz_data_descr: *mut *mut u16,
            p_optional_entropy: *const DataBlob,
            pv_reserved: *const c_void,
            p_prompt_struct: *const c_void,
            dw_flags: u32,
            p_data_out: *mut DataBlob,
        ) -> i32;
        fn LocalFree(h_mem: *mut c_void) -> *mut c_void;
    }

    /// Application entropy: binds the ciphertext to Novamail for this user, so
    /// another process running as the same user cannot decrypt it as easily.
    const ENTROPY: &[u8] = b"novamail-license-v1";

    /// Encrypt (`encrypt = true`) or decrypt a byte string with DPAPI.
    pub fn process(input: &[u8], encrypt: bool) -> Option<Vec<u8>> {
        let in_blob = DataBlob {
            cb_data: input.len() as u32,
            pb_data: input.as_ptr() as *mut u8,
        };
        let entropy_blob = DataBlob {
            cb_data: ENTROPY.len() as u32,
            pb_data: ENTROPY.as_ptr() as *mut u8,
        };
        let mut out_blob = DataBlob {
            cb_data: 0,
            pb_data: ptr::null_mut(),
        };

        let ok = unsafe {
            if encrypt {
                CryptProtectData(
                    &in_blob,
                    ptr::null(),
                    &entropy_blob,
                    ptr::null(),
                    ptr::null(),
                    0,
                    &mut out_blob,
                )
            } else {
                CryptUnprotectData(
                    &in_blob,
                    ptr::null_mut(),
                    &entropy_blob,
                    ptr::null(),
                    ptr::null(),
                    0,
                    &mut out_blob,
                )
            }
        };

        if ok == 0 || out_blob.pb_data.is_null() {
            return None;
        }
        let output = unsafe {
            std::slice::from_raw_parts(out_blob.pb_data, out_blob.cb_data as usize).to_vec()
        };
        unsafe {
            LocalFree(out_blob.pb_data as *mut c_void);
        }
        Some(output)
    }
}
