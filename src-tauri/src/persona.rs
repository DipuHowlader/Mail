//! Browser device-persona resolution.
//!
//! Every time a Chromium instance is opened from **Add profile**, it is handed a *complete new
//! machine identity*: a different OS, screen, DPR, core/memory count, touch capability and GPU,
//! drawn from a bundled catalogue of ~76 real-world devices
//! ([`resources/device-catalog.json`](../resources/device-catalog.json)). The engine switches and
//! the identity are one coherent unit, so the User-Agent, Client Hints, `navigator.platform`,
//! screen metrics, `devicePixelRatio`, WebGL strings and pointer media queries never contradict
//! each other.
//!
//! On top of the hardware identity the persona pins the **network identity** — timezone,
//! `Accept-Language`, geolocation and the WebRTC IP — to the region of the proxy's exit IP (see
//! [`crate::geo`]). Nothing is derived from this machine when a proxy is configured: a host leak
//! (local clock, host language, real WebRTC IP) is exactly what links accounts.
//!
//! Controls (all optional):
//!
//! | Variable | Meaning |
//! |---|---|
//! | `NOVAMAIL_DEVICE_EMULATION` | `0`/`off` disables the whole persona layer |
//! | `NOVAMAIL_DEVICE_POOL` | `host` (default) \| `all` \| `desktop` \| `windows,macos,linux` |
//! | `NOVAMAIL_DEVICE_SEED` | pin one fixed identity (defaults to a fresh seed per launch) |
//! | `NOVAMAIL_DEVICE_ID` | pin one catalogue entry by id |
//! | `NOVAMAIL_DEVICE_GPU_CLASS` | `intel` \| `amd` \| `nvidia` \| `apple` |
//! | `NOVAMAIL_STORAGE_QUOTA_MB` | reported `navigator.storage` quota (default 64000) |
//! | `NOVAMAIL_GEOIP` | `0` disables the egress-geo lookup |
//! | `NOVAMAIL_PROXY` / `NOVAMAIL_PROXIES` | proxy (or a list) handed to each instance |

use serde::Deserialize;
use serde_json::Value;
use std::env;

use crate::geo::{self, GeoInfo, ProxySpec};

const DEFAULT_STORAGE_QUOTA_MB: u64 = 64_000;
const CATALOG_JSON: &str = include_str!("../resources/device-catalog.json");

/// Privacy Sandbox / intrusive advertising APIs that a stealth Chromium should not answer, exactly
/// as Clearcote's own launcher disables them (a build that claims to be de-Googled while still
/// answering `document.browsingTopics()` is a pivotable contradiction). Chromium keeps only the
/// last `--disable-features`, so this must stay the only one on the automation command line.
const PRIVACY_SANDBOX_FEATURES: &[&str] = &[
    "BrowsingTopics",
    "BrowsingTopicsDocumentAPI",
    "Fledge",
    "InterestGroupStorage",
    "PrivateAggregationApi",
    "SharedStorageAPI",
    "FencedFrames",
];

#[derive(Clone, Debug, Deserialize)]
pub struct Screen {
    pub width: u32,
    pub height: u32,
    #[serde(rename = "availWidth")]
    pub avail_width: u32,
    #[serde(rename = "availHeight")]
    pub avail_height: u32,
}

/// One coherent real-world machine from the catalogue.
#[derive(Clone, Debug, Deserialize)]
pub struct Device {
    pub id: String,
    pub os: String,
    #[serde(rename = "osName")]
    pub os_name: String,
    pub model: String,
    #[serde(rename = "uaPlatform")]
    pub ua_platform: String,
    #[serde(rename = "navigatorPlatform")]
    pub navigator_platform: String,
    pub mobile: bool,
    pub brand: String,
    pub screen: Screen,
    #[serde(rename = "devicePixelRatio")]
    pub device_pixel_ratio: f64,
    #[serde(rename = "colorDepth")]
    pub color_depth: u32,
    #[serde(rename = "hardwareConcurrency")]
    pub hardware_concurrency: u32,
    #[serde(rename = "deviceMemory")]
    pub device_memory: u32,
    #[serde(rename = "maxTouchPoints")]
    pub max_touch_points: u32,
    #[serde(rename = "gpuVendor")]
    pub gpu_vendor: String,
    #[serde(rename = "gpuRenderer")]
    pub gpu_renderer: String,
}

#[derive(Deserialize)]
struct Catalog {
    devices: Vec<Device>,
}

fn catalog() -> Vec<Device> {
    serde_json::from_str::<Catalog>(CATALOG_JSON)
        .map(|catalog| catalog.devices)
        .unwrap_or_default()
}

/// The resolved identity, exposed to telemetry and the UI so an operator can see exactly which
/// machine and region a window is presenting.
#[derive(Clone, Debug)]
pub struct PersonaSpec {
    pub device_id: String,
    pub seed: String,
    pub os: String,
    pub os_name: String,
    pub model: String,
    pub ua_platform: String,
    pub navigator_platform: String,
    pub mobile: bool,
    pub screen_width: u32,
    pub screen_height: u32,
    pub avail_width: u32,
    pub avail_height: u32,
    pub device_pixel_ratio: f64,
    pub color_depth: u32,
    pub hardware_concurrency: u32,
    pub device_memory: u32,
    pub max_touch_points: u32,
    pub gpu_vendor: String,
    pub gpu_renderer: String,
    pub timezone: String,
    pub accept_language: String,
    pub languages: Vec<String>,
    pub location: Option<String>,
    pub webrtc_ip: Option<String>,
    pub proxy: Option<String>,
}

impl PersonaSpec {
    /// One-line description for logs/telemetry: `Windows · Dell XPS 13 9310 · 1920x1200 @1.25 · Europe/Berlin`.
    pub fn summary(&self) -> String {
        format!(
            "{} · {} · {}x{} @{} · {}",
            self.os_name,
            self.model,
            self.screen_width,
            self.screen_height,
            self.device_pixel_ratio,
            self.timezone
        )
    }

    /// The JSON the renderer receives on profile creation.
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "deviceId": self.device_id,
            "os": self.os,
            "osName": self.os_name,
            "model": self.model,
            "platform": self.ua_platform,
            "navigatorPlatform": self.navigator_platform,
            "mobile": self.mobile,
            "screen": {
                "width": self.screen_width,
                "height": self.screen_height,
                "availWidth": self.avail_width,
                "availHeight": self.avail_height,
            },
            "devicePixelRatio": self.device_pixel_ratio,
            "colorDepth": self.color_depth,
            "hardwareConcurrency": self.hardware_concurrency,
            "deviceMemory": self.device_memory,
            "maxTouchPoints": self.max_touch_points,
            "gpuVendor": self.gpu_vendor,
            "gpuRenderer": self.gpu_renderer,
            "timezone": self.timezone,
            "acceptLanguage": self.accept_language,
            "languages": self.languages,
            "location": self.location,
            "webrtcIp": self.webrtc_ip,
            "proxy": self.proxy,
            "summary": self.summary(),
        })
    }
}

/// A launched persona: the engine switches, the window geometry it should open at, and the identity
/// itself.
#[derive(Clone, Debug)]
pub struct Persona {
    pub args: Vec<String>,
    pub window: Option<(u32, u32)>,
    pub spec: PersonaSpec,
}

fn device_emulation_enabled() -> bool {
    match env::var("NOVAMAIL_DEVICE_EMULATION") {
        Ok(raw) => !matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no" | "disabled"
        ),
        Err(_) => true,
    }
}

fn host_platform() -> &'static str {
    match env::consts::OS {
        "windows" => "windows",
        "macos" => "macos",
        "linux" => "linux",
        _ => "windows",
    }
}

/// Which operating systems the catalogue draw is allowed to use. Defaults to **all three**, so each
/// launch presents a different OS class; set `NOVAMAIL_DEVICE_POOL=host` to stay on this machine's
/// OS (native fonts/canvas/GPU limits — the most conservative choice).
fn device_pool() -> Vec<String> {
    let raw = env::var("NOVAMAIL_DEVICE_POOL").unwrap_or_else(|_| "host".to_string());
    let host = host_platform();
    let mut pool: Vec<String> = Vec::new();
    let push = |pool: &mut Vec<String>, value: &str| {
        if !pool.iter().any(|entry| entry == value) {
            pool.push(value.to_string());
        }
    };
    for token in raw.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        match token.to_ascii_lowercase().as_str() {
            "host" | "auto" | "native" => push(&mut pool, host),
            "desktop" => {
                for os in ["windows", "macos", "linux"] {
                    push(&mut pool, os);
                }
            }
            "all" => {
                for os in ["windows", "macos", "linux"] {
                    push(&mut pool, os);
                }
            }
            other @ ("windows" | "macos" | "linux") => push(&mut pool, other),
            _ => {}
        }
    }
    if pool.is_empty() {
        push(&mut pool, host);
    }
    pool
}

fn gpu_class_filter() -> Option<String> {
    env::var("NOVAMAIL_DEVICE_GPU_CLASS")
        .ok()
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
}

/// A deterministic 64-bit hash (FNV-1a) so a seed always selects the same device.
fn seed_hash(seed: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in seed.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn resolve_seed(options: &Value) -> String {
    for key in ["deviceSeed", "seed"] {
        if let Some(value) = options.get(key).and_then(Value::as_str) {
            if !value.trim().is_empty() {
                return value.trim().to_string();
            }
        }
    }
    for name in ["NOVAMAIL_DEVICE_SEED", "NOVAMAIL_MOBILE_FINGERPRINT"] {
        if let Ok(value) = env::var(name) {
            let value = value.trim().to_string();
            if !value.is_empty() {
                return value;
            }
        }
    }
    format!("novamail-{}", uuid::Uuid::new_v4())
}

fn device_matches_pool(device: &Device, pool: &[String]) -> bool {
    pool.iter().any(|os| os == &device.os)
}

fn device_matches_gpu_class(device: &Device, class: &str) -> bool {
    let vendor = device.gpu_vendor.to_ascii_lowercase();
    match class {
        other => vendor.contains(other),
    }
}

/// Pick one device: the pool first, then the optional GPU-class filter, then the seed's index.
fn select_device(
    devices: &[Device],
    seed: &str,
    options: &Value,
    pool: &[String],
    gpu_class: Option<&str>,
) -> Option<Device> {
    let mut candidates: Vec<&Device> = devices
        .iter()
        .filter(|device| device_matches_pool(device, pool))
        .collect();
    if candidates.is_empty() {
        candidates = devices.iter().collect();
    }
    if let Some(class) = gpu_class {
        let filtered: Vec<&Device> = candidates
            .iter()
            .copied()
            .filter(|device| device_matches_gpu_class(device, &class))
            .collect();
        if !filtered.is_empty() {
            candidates = filtered;
        }
    }
    // An explicit device id wins over the draw (used to reproduce one identity).
    let pinned = options
        .get("deviceId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| env::var("NOVAMAIL_DEVICE_ID").ok());
    if let Some(pinned) = pinned.filter(|id| !id.trim().is_empty()) {
        if let Some(device) = candidates.iter().find(|device| device.id == pinned.trim()) {
            return Some((*device).clone());
        }
    }
    if candidates.is_empty() {
        return None;
    }
    let index = (seed_hash(seed) % candidates.len() as u64) as usize;
    Some(candidates[index].clone())
}

fn storage_quota_mb() -> u64 {
    env::var("NOVAMAIL_STORAGE_QUOTA_MB")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_STORAGE_QUOTA_MB)
}

/// Resolve the proxy a window should use: an explicit `options.proxy` first, then the configured
/// list, chosen deterministically from the seed so a re-launch of the same seed keeps its egress.
fn resolve_proxy(options: &Value, seed: &str) -> Option<ProxySpec> {
    if let Some(value) = options.get("proxy") {
        let parsed = match value {
            Value::String(url) => ProxySpec::parse(url),
            Value::Object(map) => {
                let server = map.get("server").and_then(Value::as_str).unwrap_or("");
                match ProxySpec::parse(server) {
                    Some(mut proxy) => {
                        if let Some(username) = map.get("username").and_then(Value::as_str) {
                            if !username.is_empty() {
                                proxy.username = username.to_string();
                            }
                        }
                        if let Some(password) = map.get("password").and_then(Value::as_str) {
                            if !password.is_empty() {
                                proxy.password = password.to_string();
                            }
                        }
                        Some(proxy)
                    }
                    None => None,
                }
            }
            _ => None,
        };
        if parsed.is_some() {
            return parsed;
        }
    }
    let proxies = geo::configured_proxies();
    if proxies.is_empty() {
        return None;
    }
    let index = (seed_hash(seed) % proxies.len() as u64) as usize;
    Some(proxies[index].clone())
}

fn privacy_args() -> Vec<String> {
    vec![format!("--disable-features={}", PRIVACY_SANDBOX_FEATURES.join(","))]
}

/// Build every engine switch for one device + one geo. Each switch is emitted exactly once: the
/// engine exposes its command line over CDP, and a flag repeated is a shape no real browser has.
/// The language the browser presents.
///
/// English by default — the operator always gets an English browser, whether or
/// not a proxy is configured. `NOVAMAIL_ACCEPT_LANG` pins a specific tag when
/// the network identity must match a proxy's exit region (the geo layer still
/// drives the clock, coordinates and WebRTC IP either way).
fn preferred_language() -> String {
    std::env::var("NOVAMAIL_ACCEPT_LANG")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "en-US".to_string())
}

fn engine_args(
    device: &Device,
    seed: &str,
    geo: Option<&GeoInfo>,
    storage_quota_mb: u64,
    accept_language: &str,
) -> Vec<String> {
    let mut args = Vec::new();
    args.push(format!("--fingerprint={seed}"));
    args.push(format!("--fingerprint-platform={}", device.os));
    args.push(format!("--fingerprint-brand={}", device.brand));
    args.push(format!("--fingerprint-gpu-vendor={}", device.gpu_vendor));
    args.push(format!("--fingerprint-gpu-renderer={}", device.gpu_renderer));
    args.push(format!("--fingerprint-hardware-concurrency={}", device.hardware_concurrency));
    args.push(format!("--fingerprint-device-memory={}", device.device_memory));
    args.push(format!("--fingerprint-screen-width={}", device.screen.width));
    args.push(format!("--fingerprint-screen-height={}", device.screen.height));
    args.push(format!("--fingerprint-avail-width={}", device.screen.avail_width));
    args.push(format!("--fingerprint-avail-height={}", device.screen.avail_height));
    args.push(format!("--fingerprint-color-depth={}", device.color_depth));
    args.push(format!("--fingerprint-device-pixel-ratio={}", device.device_pixel_ratio));
    args.push(format!("--fingerprint-max-touch-points={}", device.max_touch_points));
    args.push(format!("--fingerprint-storage-quota={storage_quota_mb}"));

    // Network identity: the proxy's region, never this machine's. The language
    // is English by default (see `preferred_language`); the timezone, location
    // and WebRTC IP still follow the egress.
    let (accept, ui) = geo::resolve_languages(accept_language);
    args.push(format!("--accept-lang={accept}"));
    if let Some(ui) = ui {
        args.push(format!("--lang={ui}"));
    }
    let timezone = geo
        .and_then(|geo| geo.timezone.clone())
        .unwrap_or_else(|| geo::default_timezone(accept.split(',').next().unwrap_or("en-US")));
    args.push(format!("--timezone={timezone}"));
    if let Some(location) = geo.and_then(|geo| geo.location.clone()) {
        args.push(format!("--fingerprint-location={location}"));
    }
    if let Some(ip) = geo.and_then(|geo| geo.ip.clone()) {
        // The engine fabricates the WebRTC srflx candidate at this IP and sends no real STUN, so
        // the real address never leaks at the packet level.
        args.push(format!("--webrtc-ip={ip}"));
    }

    args.extend(privacy_args());

    // Pin the window to the claimed panel so `innerWidth/Height`, `outerWidth/Height` and the
    // screen agree the way a real machine's window does. `--window-size` is added by the driver
    // (see `DriverConfig::window_size`) from `Persona::window`, never here, so it appears once.
    args
}

fn language_list(accept: &str) -> Vec<String> {
    accept
        .split(',')
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .map(str::to_string)
        .collect()
}

/// Resolve a full persona for one browser launch.
pub async fn resolve(options: &Value) -> Persona {
    let seed = resolve_seed(options);
    let devices = catalog();
    let pool = device_pool();
    let gpu_class = gpu_class_filter();
    let device = select_device(&devices, &seed, options, &pool, gpu_class.as_deref());
    let Some(device) = device else {
        return Persona {
            args: Vec::new(),
            window: None,
            spec: PersonaSpec {
                device_id: String::new(),
                seed: seed.clone(),
                os: host_platform().to_string(),
                os_name: String::new(),
                model: String::new(),
                ua_platform: String::new(),
                navigator_platform: String::new(),
                mobile: false,
                screen_width: 0,
                screen_height: 0,
                avail_width: 0,
                avail_height: 0,
                device_pixel_ratio: 1.0,
                color_depth: 24,
                hardware_concurrency: 0,
                device_memory: 0,
                max_touch_points: 0,
                gpu_vendor: String::new(),
                gpu_renderer: String::new(),
                timezone: "America/New_York".to_string(),
                accept_language: "en-US".to_string(),
                languages: vec!["en-US".to_string(), "en".to_string()],
                location: None,
                webrtc_ip: None,
                proxy: None,
            },
        };
    };

    let proxy = resolve_proxy(options, &seed);
    let geo = geo::resolve_geo(proxy.as_ref()).await;

    let accept_language = preferred_language();
    let (accept, _) = geo::resolve_languages(&accept_language);
    let timezone = geo
        .as_ref()
        .and_then(|geo| geo.timezone.clone())
        .unwrap_or_else(|| geo::default_timezone(accept.split(',').next().unwrap_or("en-US")));

    let spec = PersonaSpec {
        device_id: device.id.clone(),
        seed: seed.clone(),
        os: device.os.clone(),
        os_name: device.os_name.clone(),
        model: device.model.clone(),
        ua_platform: device.ua_platform.clone(),
        navigator_platform: device.navigator_platform.clone(),
        mobile: device.mobile,
        screen_width: device.screen.width,
        screen_height: device.screen.height,
        avail_width: device.screen.avail_width,
        avail_height: device.screen.avail_height,
        device_pixel_ratio: device.device_pixel_ratio,
        color_depth: device.color_depth,
        hardware_concurrency: device.hardware_concurrency,
        device_memory: device.device_memory,
        max_touch_points: device.max_touch_points,
        gpu_vendor: device.gpu_vendor.clone(),
        gpu_renderer: device.gpu_renderer.clone(),
        timezone,
        accept_language: accept.clone(),
        languages: language_list(&accept),
        location: geo.as_ref().and_then(|geo| geo.location.clone()),
        webrtc_ip: geo.as_ref().and_then(|geo| geo.ip.clone()),
        proxy: proxy.as_ref().map(ProxySpec::label),
    };

    if !device_emulation_enabled() {
        return Persona {
            args: Vec::new(),
            window: Some((spec.avail_width, spec.avail_height)),
            spec,
        };
    }

    let mut args = engine_args(&device, &seed, geo.as_ref(), storage_quota_mb(), &accept_language);
    if let Some(proxy) = &proxy {
        args.extend(proxy.engine_args(geo::proxy_bypass().as_deref()));
    }

    Persona {
        args,
        window: Some((device.screen.avail_width, device.screen.avail_height)),
        spec,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_every_os() {
        let devices = catalog();
        assert!(devices.len() >= 70, "expected a large catalogue, got {}", devices.len());
        for os in ["windows", "macos", "linux"] {
            assert!(devices.iter().any(|device| device.os == os), "missing {os}");
        }
    }

    #[test]
    fn selection_is_deterministic_and_in_pool() {
        let devices = catalog();
        let options = serde_json::json!({});
        let pool = vec!["windows".to_string()];
        let a = select_device(&devices, "seed-1", &options, &pool, None).unwrap();
        let b = select_device(&devices, "seed-1", &options, &pool, None).unwrap();
        let c = select_device(&devices, "seed-2", &options, &pool, None).unwrap();
        assert_eq!(a.id, b.id);
        assert_ne!(a.id, c.id);
        assert_eq!(a.os, "windows");
        assert_eq!(c.os, "windows");
    }

    #[test]
    fn args_are_single_and_coherent() {
        let devices = catalog();
        let device = devices.iter().find(|d| d.os == "macos").unwrap().clone();
        let geo = GeoInfo {
            ip: Some("203.0.113.10".to_string()),
            country: Some("DE".to_string()),
            accept_language: Some("de-DE".to_string()),
            timezone: Some("Europe/Berlin".to_string()),
            location: Some("52.52,13.405".to_string()),
        };
        let args = engine_args(&device, "seed", Some(&geo), 64_000, "en-US");
        assert_eq!(args.iter().filter(|a| a.starts_with("--fingerprint=")).count(), 1);
        assert!(args.contains(&"--fingerprint-platform=macos".to_string()));
        assert!(args.contains(&"--timezone=Europe/Berlin".to_string()));
        // The language defaults to English regardless of the egress country.
        assert!(args.contains(&"--accept-lang=en-US,en".to_string()));
        assert!(args.contains(&"--lang=en-US".to_string()));
        assert!(args.contains(&"--webrtc-ip=203.0.113.10".to_string()));
        assert!(args.contains(&"--fingerprint-location=52.52,13.405".to_string()));

        // A proxy-matched identity can still pin the egress locale explicitly.
        let args = engine_args(&device, "seed", Some(&geo), 64_000, "de-DE");
        assert!(args.contains(&"--accept-lang=de-DE,de,en-US,en".to_string()));
        assert!(args.contains(&"--lang=de".to_string()));
    }
}
