//! Egress geography, proxy plumbing and locale resolution.
//!
//! A browser persona is only coherent if its **clock, language, coordinates and WebRTC IP agree
//! with the address the sites actually see**. That address is the proxy's exit IP, not this
//! machine's, so every lookup here goes *through the proxy* — never around it.
//!
//! The behaviour mirrors Clearcote's own `geoip` directive:
//!
//! 1. discover the exit IP with a small IP-echo *through the proxy* (`api.ipify.org`, then
//!    `icanhazip.com`, or `CLEARCOTE_GEOIP_ECHO_URL`),
//! 2. look that IP up (the exit IP as `ipwho.is` / `ipinfo.io` reports it when reached through the
//!    proxy, so the answer is already the exit's region),
//! 3. derive the OS locale for the country, expand it to Chrome's own language list, and pin the
//!    IANA timezone, the coordinates and the WebRTC IP.
//!
//! `NOVAMAIL_GEOIP=0` disables the layer. `CLEARCOTE_GEOIP_*` variables are honoured so a machine
//! already configured for Clearcote behaves the same here.

use std::time::Duration;

use serde_json::Value;

/// A parsed proxy URL: `scheme://[user[:pass]@]host:port`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxySpec {
    /// `http`, `https`, `socks5`, `socks5h` or `socks4`.
    pub scheme: String,
    /// `scheme://host:port`, with any userinfo stripped (Chromium rejects userinfo in
    /// `--proxy-server`).
    pub server: String,
    pub username: String,
    pub password: String,
}

impl ProxySpec {
    /// Parse a proxy URL. Accepts the `host:port` shorthand (treated as `http`). Returns `None`
    /// for an empty or malformed value.
    pub fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            return None;
        }
        let with_scheme = if raw.contains("://") {
            raw.to_string()
        } else {
            format!("http://{raw}")
        };
        let (scheme, rest) = with_scheme.split_once("://")?;
        let scheme = scheme.to_ascii_lowercase();
        if !matches!(scheme.as_str(), "http" | "https" | "socks5" | "socks5h" | "socks4" | "socks4a") {
            return None;
        }
        // Split userinfo at the LAST '@' so an unescaped '@' in a password survives.
        let (userinfo, authority) = match rest.rfind('@') {
            Some(at) => (&rest[..at], &rest[at + 1..]),
            None => ("", rest),
        };
        let authority = authority.split('/').next().unwrap_or(authority).trim();
        if authority.is_empty() {
            return None;
        }
        let (username, password) = match userinfo.split_once(':') {
            Some((user, pass)) => (decode_userinfo(user), decode_userinfo(pass)),
            None => (decode_userinfo(userinfo), String::new()),
        };
        Some(Self {
            server: format!("{scheme}://{authority}"),
            scheme,
            username,
            password,
        })
    }

    fn is_socks(&self) -> bool {
        self.scheme.starts_with("socks")
    }

    /// `socks5h://user:pass@host:port` for reqwest (remote DNS), the URL form for http(s).
    fn reqwest_url(&self) -> String {
        let scheme = if self.scheme == "socks5" { "socks5h" } else { &self.scheme };
        let plain = self.server.strip_prefix(&format!("{}://", self.scheme)).unwrap_or(&self.server);
        if self.username.is_empty() && self.password.is_empty() {
            format!("{scheme}://{plain}")
        } else {
            format!(
                "{scheme}://{}:{}@{}",
                encode_userinfo(&self.username),
                encode_userinfo(&self.password),
                plain
            )
        }
    }

    /// The Chromium flags that route every request through this proxy.
    ///
    /// Credentials never go in `--proxy-server` (Chromium's proxy parser drops a URL that carries
    /// them). `--socks5-credentials` is the engine's own RFC 1929 authentication, which stock
    /// Chromium lacks; `--proxy-auth` is its HTTP(S) equivalent.
    pub fn engine_args(&self, bypass: Option<&str>) -> Vec<String> {
        let mut args = vec![format!("--proxy-server={}", self.server)];
        if !self.username.is_empty() || !self.password.is_empty() {
            let flag = if self.is_socks() { "--socks5-credentials" } else { "--proxy-auth" };
            args.push(format!("{flag}={}:{}", self.username, self.password));
        }
        if let Some(bypass) = bypass.map(str::trim).filter(|b| !b.is_empty()) {
            args.push(format!("--proxy-bypass-list={bypass}"));
        }
        // A SOCKS/HTTP proxy carries TCP only, so real Chrome never negotiates QUIC through one.
        args.push("--disable-quic".to_string());
        args
    }

    /// A redacted form for logs (never the credentials).
    pub fn label(&self) -> String {
        self.server.clone()
    }
}

fn decode_userinfo(value: &str) -> String {
    percent_decode(value)
}

fn encode_userinfo(value: &str) -> String {
    // Only the characters that would corrupt the authority need escaping.
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The country-language table Clearcote ships (ISO-3166 alpha-2 → the OS locale a machine there
/// most plausibly runs).
fn accept_language_for_country(country: &str) -> String {
    let cc = country.trim().to_ascii_uppercase();
    let tag = match cc.as_str() {
        "US" => "en-US", "GB" => "en-GB", "CA" => "en-CA", "AU" => "en-AU",
        "NZ" => "en-NZ", "IE" => "en-IE", "IN" => "en-IN", "ZA" => "en-ZA",
        "SG" => "en-SG", "PH" => "en-PH",
        "DE" => "de-DE", "AT" => "de-AT", "CH" => "de-CH", "FR" => "fr-FR",
        "BE" => "nl-BE", "NL" => "nl-NL",
        "ES" => "es-ES", "MX" => "es-MX", "AR" => "es-AR", "CL" => "es-CL",
        "CO" => "es-CO", "PE" => "es-PE", "VE" => "es-VE", "EC" => "es-EC",
        "UY" => "es-UY", "PT" => "pt-PT", "BR" => "pt-BR",
        "IT" => "it-IT", "PL" => "pl-PL", "RU" => "ru-RU", "UA" => "uk-UA",
        "SE" => "sv-SE", "NO" => "nb-NO", "DK" => "da-DK", "FI" => "fi-FI",
        "CZ" => "cs-CZ", "SK" => "sk-SK", "RO" => "ro-RO", "HU" => "hu-HU",
        "GR" => "el-GR", "BG" => "bg-BG", "HR" => "hr-HR", "SI" => "sl-SI",
        "RS" => "sr-RS", "LT" => "lt-LT", "LV" => "lv-LV", "EE" => "et-EE",
        "TR" => "tr-TR", "IL" => "he-IL", "SA" => "ar-SA", "AE" => "ar-AE",
        "EG" => "ar-EG", "JP" => "ja-JP", "KR" => "ko-KR", "CN" => "zh-CN",
        "HK" => "zh-HK", "TW" => "zh-TW", "TH" => "th-TH", "VN" => "vi-VN",
        "ID" => "id-ID", "MY" => "ms-MY", "BD" => "bn-BD", "PK" => "ur-PK",
        _ => "en-US",
    };
    tag.to_string()
}

/// `navigator.languages` + `Accept-Language` header, and the UI locale for `--lang`.
///
/// Ports Clearcote's `resolveLanguages`: one tag is read as the OS locale and expanded to Chrome's
/// own default list for it (`de-AT` → `de-DE,de,en-US,en`, UI `de`); a comma list is kept verbatim.
pub fn resolve_languages(accept_language: &str) -> (String, Option<String>) {
    let clean = accept_language
        .split(',')
        .map(|part| part.split(';').next().unwrap_or(part).trim())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(",");
    let tags: Vec<&str> = clean.split(',').filter(|t| !t.is_empty()).collect();
    let Some(first) = tags.first() else {
        return (clean, None);
    };
    let ui = chrome_ui_locale(first);
    if tags.len() == 1 {
        if let Some(ui) = &ui {
            return (chrome_accept_languages(ui).to_string(), Some(ui.clone()));
        }
    }
    (clean.clone(), ui.or_else(|| Some((*first).to_string())))
}

fn split_tag(tag: &str) -> (String, String, String) {
    let parts: Vec<&str> = tag
        .split(['-', '_'])
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    let Some(first) = parts.first() else {
        return (String::new(), String::new(), String::new());
    };
    let alias = match first.to_ascii_lowercase().as_str() {
        "iw" => "he",
        "in" => "id",
        "tl" => "fil",
        "no" | "nn" => "nb",
        other => other,
    }
    .to_string();
    let mut script = String::new();
    let mut region = String::new();
    for part in &parts[1..] {
        if part.len() == 4 && part.chars().all(|c| c.is_ascii_alphabetic()) {
            let mut chars = part.chars();
            if let Some(head) = chars.next() {
                script = format!("{}{}", head.to_ascii_uppercase(), chars.as_str().to_ascii_lowercase());
            }
        } else if (part.len() == 2 && part.chars().all(|c| c.is_ascii_alphabetic()))
            || (part.len() == 3 && part.chars().all(|c| c.is_ascii_digit()))
        {
            region = part.to_ascii_uppercase();
        }
    }
    (alias, script, region)
}

fn chrome_ui_locale(tag: &str) -> Option<String> {
    let (lang, script, region) = split_tag(tag);
    if lang.is_empty() {
        return None;
    }
    let ui = match lang.as_str() {
        "en" => {
            if matches!(region.as_str(), "" | "US" | "PH" | "AS" | "GU" | "MH" | "MP" | "PR" | "UM" | "VI") {
                "en-US"
            } else {
                "en-GB"
            }
        }
        "es" => {
            if matches!(region.as_str(), "" | "ES" | "EA" | "IC" | "GQ") {
                "es"
            } else {
                "es-419"
            }
        }
        "pt" => {
            if region.is_empty() || region == "BR" {
                "pt-BR"
            } else {
                "pt-PT"
            }
        }
        "zh" => {
            let traditional = script == "Hant" || (script != "Hans" && matches!(region.as_str(), "TW" | "HK" | "MO"));
            if traditional {
                "zh-TW"
            } else {
                "zh-CN"
            }
        }
        other => {
            if CHROME_UI_LOCALES.contains(&other) {
                other
            } else {
                return None;
            }
        }
    };
    Some(ui.to_string())
}

fn chrome_accept_languages(ui: &str) -> &'static str {
    match ui {
        "am" => "am,en-GB,en", "ar" => "ar,en-US,en", "bg" => "bg-BG,bg",
        "bn" => "bn-IN,bn,en-US,en", "ca" => "ca-ES,ca", "cs" => "cs-CZ,cs",
        "da" => "da-DK,da,en-US,en", "de" => "de-DE,de,en-US,en", "el" => "el-GR,el",
        "en-GB" => "en-GB,en-US,en", "es" => "es-ES,es", "es-419" => "es-419,es",
        "et" => "et-EE,et,en-US,en", "fa" => "fa,en-US,en", "fi" => "fi-FI,fi,en-US,en",
        "fil" => "fil,fil-PH,tl,en-US,en", "fr" => "fr-FR,fr,en-US,en",
        "gu" => "gu-IN,gu,hi-IN,hi,en-US,en", "he" => "he-IL,he,en-US,en",
        "hi" => "hi-IN,hi,en-US,en", "hr" => "hr-HR,hr,en-US,en", "hu" => "hu-HU,hu,en-US,en",
        "id" => "id-ID,id,en-US,en", "it" => "it-IT,it,en-US,en", "ja" => "ja,en-US,en",
        "kn" => "kn-IN,kn,en-US,en", "ko" => "ko-KR,ko,en-US,en",
        "lt" => "lt,en-US,en,ru,pl", "lv" => "lv-LV,lv,en-US,en", "ml" => "ml-IN,ml,en-US,en",
        "mr" => "mr-IN,mr,hi-IN,hi,en-US,en", "nb" => "nb-NO,nb,no,nn,en-US,en",
        "nl" => "nl-NL,nl,en-US,en", "pl" => "pl-PL,pl,en-US,en", "pt-BR" => "pt-BR,pt,en-US,en",
        "pt-PT" => "pt-PT,pt,en-US,en", "ro" => "ro-RO,ro,en-US,en", "ru" => "ru-RU,ru,en-US,en",
        "sk" => "sk-SK,sk,cs,en-US,en", "sl" => "sl-SI,sl,en-GB,en", "sr" => "sr-RS,sr,en-US,en",
        "sv" => "sv-SE,sv,en-US,en", "sw" => "sw,en-GB,en", "ta" => "ta-IN,ta,en-US,en",
        "te" => "te-IN,te,hi-IN,hi,en-US,en", "th" => "th-TH,th", "tr" => "tr-TR,tr,en-US,en",
        "uk" => "uk-UA,uk,en-US,en", "vi" => "vi-VN,vi,fr-FR,fr,en-US,en",
        "zh-CN" => "zh-CN,zh", "zh-TW" => "zh-TW,zh,en-US,en",
        _ => "en-US,en",
    }
}

const CHROME_UI_LOCALES: &[&str] = &[
    "af", "am", "ar", "bg", "bn", "ca", "cs", "da", "de", "el", "en-GB", "en-US", "es", "es-419",
    "et", "fa", "fi", "fil", "fr", "gu", "he", "hi", "hr", "hu", "id", "it", "ja", "kn", "ko",
    "lt", "lv", "ml", "mr", "ms", "nb", "nl", "pl", "pt-BR", "pt-PT", "ro", "ru", "sk", "sl", "sr",
    "sv", "sw", "ta", "te", "th", "tr", "uk", "ur", "vi", "zh-CN", "zh-TW",
];

/// A timezone coherent with a primary language tag, so a failed lookup leaks neither the host's
/// UTC nor a locale that contradicts `navigator.language`. Mirrors Clearcote's `defaultTimezone`.
pub fn default_timezone(primary_language: &str) -> String {
    let tag = primary_language.trim().replace('_', "-");
    let mut parts: Vec<String> = tag.split('-').map(str::to_string).collect();
    if parts.len() >= 2 {
        let last = parts.last().cloned().unwrap_or_default();
        if last.len() == 2 {
            parts[0] = parts[0].to_ascii_lowercase();
            let upper = last.to_ascii_uppercase();
            let n = parts.len();
            parts[n - 1] = upper;
        }
    }
    let tag = parts.join("-");
    match tag.as_str() {
        "en-US" => "America/New_York", "en-CA" => "America/Toronto", "en-GB" => "Europe/London",
        "en-AU" => "Australia/Sydney", "en-NZ" => "Pacific/Auckland", "en-IE" => "Europe/Dublin",
        "de-DE" => "Europe/Berlin", "de-AT" => "Europe/Vienna", "fr-FR" => "Europe/Paris",
        "es-ES" => "Europe/Madrid", "es-MX" => "America/Mexico_City", "it-IT" => "Europe/Rome",
        "nl-NL" => "Europe/Amsterdam", "pt-BR" => "America/Sao_Paulo", "pt-PT" => "Europe/Lisbon",
        "pl-PL" => "Europe/Warsaw", "sv-SE" => "Europe/Stockholm", "ja-JP" => "Asia/Tokyo",
        "ko-KR" => "Asia/Seoul", "zh-CN" => "Asia/Shanghai", "zh-TW" => "Asia/Taipei",
        "ru-RU" => "Europe/Moscow", "tr-TR" => "Europe/Istanbul", "ar-SA" => "Asia/Riyadh",
        "hi-IN" => "Asia/Kolkata", "id-ID" => "Asia/Jakarta", "en-IN" => "Asia/Kolkata",
        "en-ZA" => "Africa/Johannesburg", "en-SG" => "Asia/Singapore", "en-PH" => "Asia/Manila",
        "de-CH" => "Europe/Zurich", "fr-CA" => "America/Toronto", "nl-BE" => "Europe/Brussels",
        "es-AR" => "America/Argentina/Buenos_Aires", "es-CL" => "America/Santiago",
        "es-CO" => "America/Bogota", "uk-UA" => "Europe/Kyiv", "nb-NO" => "Europe/Oslo",
        "da-DK" => "Europe/Copenhagen", "fi-FI" => "Europe/Helsinki", "cs-CZ" => "Europe/Prague",
        "ro-RO" => "Europe/Bucharest", "hu-HU" => "Europe/Budapest", "el-GR" => "Europe/Athens",
        "he-IL" => "Asia/Jerusalem", "ar-AE" => "Asia/Dubai", "ar-EG" => "Africa/Cairo",
        "zh-HK" => "Asia/Hong_Kong", "th-TH" => "Asia/Bangkok", "vi-VN" => "Asia/Ho_Chi_Minh",
        "ms-MY" => "Asia/Kuala_Lumpur", "bg-BG" => "Europe/Sofia", "hr-HR" => "Europe/Zagreb",
        "sk-SK" => "Europe/Bratislava", "sl-SI" => "Europe/Ljubljana", "sr-RS" => "Europe/Belgrade",
        "lt-LT" => "Europe/Vilnius", "lv-LV" => "Europe/Riga", "et-EE" => "Europe/Tallinn",
        "bn-BD" => "Asia/Dhaka", "ur-PK" => "Asia/Karachi",
        _ => {
            let lang = tag.split('-').next().unwrap_or("").to_ascii_lowercase();
            match lang.as_str() {
                "en" => "America/New_York", "de" => "Europe/Berlin", "fr" => "Europe/Paris",
                "es" => "Europe/Madrid", "pt" => "America/Sao_Paulo", "it" => "Europe/Rome",
                "nl" => "Europe/Amsterdam", "ru" => "Europe/Moscow", "ja" => "Asia/Tokyo",
                "ko" => "Asia/Seoul", "zh" => "Asia/Shanghai", "ar" => "Asia/Riyadh",
                "hi" => "Asia/Kolkata", "bn" => "Asia/Dhaka", "ur" => "Asia/Karachi",
                _ => "America/New_York",
            }
        }
    }
    .to_string()
}

/// What the lookup resolved for the egress.
#[derive(Clone, Debug, Default)]
pub struct GeoInfo {
    pub ip: Option<String>,
    /// ISO country code of the egress. Kept for diagnostics even though only its *derived* locale
    /// (`accept_language`) reaches the engine.
    #[allow(dead_code)]
    pub country: Option<String>,
    /// One OS locale tag (`en-US`, `de-DE`), already country-derived.
    pub accept_language: Option<String>,
    pub timezone: Option<String>,
    /// `"lat,lon"` for `--fingerprint-location`.
    pub location: Option<String>,
}

impl GeoInfo {
    pub fn is_usable(&self) -> bool {
        self.timezone.is_some()
    }
}

fn geo_enabled() -> bool {
    match std::env::var("NOVAMAIL_GEOIP") {
        Ok(raw) => !matches!(raw.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no" | "disabled"),
        Err(_) => true,
    }
}

fn env_ms(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .unwrap_or(default)
}

fn geoip_timeout_ms() -> u64 {
    if let Ok(seconds) = std::env::var("CLEARCOTE_GEOIP_TIMEOUT_SECONDS") {
        if let Ok(value) = seconds.trim().parse::<f64>() {
            if value > 0.0 {
                return (value * 1000.0).round() as u64;
            }
        }
    }
    env_ms("NOVAMAIL_GEO_TIMEOUT_MS", 8_000)
}

fn echo_urls() -> Vec<String> {
    let custom = std::env::var("CLEARCOTE_GEOIP_ECHO_URL").unwrap_or_default();
    let urls: Vec<String> = custom
        .split(',')
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .map(str::to_string)
        .collect();
    if !urls.is_empty() {
        return urls;
    }
    vec![
        "https://api.ipify.org".to_string(),
        "https://icanhazip.com".to_string(),
    ]
}

fn ipapi_enabled() -> bool {
    matches!(
        std::env::var("CLEARCOTE_GEOIP_IPAPI")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn build_client(proxy: Option<&ProxySpec>, timeout_ms: u64) -> Option<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .redirect(reqwest::redirect::Policy::limited(5));
    if let Some(proxy_spec) = proxy {
        match reqwest::Proxy::all(proxy_spec.reqwest_url()) {
            Ok(reqwest_proxy) => builder = builder.proxy(reqwest_proxy),
            Err(_) => return None,
        }
    }
    // A proxy lookup must not fall back to the host's direct connection: that would resolve the
    // wrong region. reqwest only uses the proxy when one is configured, so nothing else is needed.
    builder.build().ok()
}

fn looks_like_ip(candidate: &str) -> bool {
    let candidate = candidate.trim();
    if candidate.is_empty() || candidate.len() > 45 {
        return false;
    }
    if candidate.contains(':') {
        return candidate.chars().all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.');
    }
    let parts: Vec<&str> = candidate.split('.').collect();
    parts.len() == 4 && parts.iter().all(|p| p.parse::<u8>().is_ok())
}

/// The exit IP as seen through `proxy` (or directly when there is none).
async fn exit_ip(proxy: Option<&ProxySpec>, timeout_ms: u64) -> Option<String> {
    for url in echo_urls() {
        let Some(client) = build_client(proxy, timeout_ms.min(8_000)) else {
            continue;
        };
        let Ok(response) = client.get(&url).send().await else {
            continue;
        };
        let Ok(text) = response.text().await else {
            continue;
        };
        let text = text.trim();
        let value = if text.starts_with('{') {
            serde_json::from_str::<Value>(text)
                .ok()
                .and_then(|json| {
                    json.get("ip")
                        .or_else(|| json.get("query"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
        } else {
            Some(text.split_whitespace().next().unwrap_or("").to_string())
        };
        if let Some(ip) = value.filter(|ip| looks_like_ip(ip)) {
            return Some(ip);
        }
    }
    None
}

fn geo_from_ipwho(json: &Value) -> GeoInfo {
    if json.get("success").and_then(Value::as_bool) == Some(false) {
        return GeoInfo::default();
    }
    let country = json.get("country_code").and_then(Value::as_str).map(str::to_string);
    let timezone = json
        .get("timezone")
        .and_then(|tz| tz.get("id").and_then(Value::as_str).or_else(|| tz.as_str()))
        .map(str::to_string);
    // ipwho.is reports the location as its own object; older responses use top-level lat/lon.
    let (lat, lon) = match (json.get("latitude"), json.get("longitude")) {
        (Some(lat), Some(lon)) => (lat.as_f64(), lon.as_f64()),
        _ => (
            json.get("location").and_then(|l| l.get("latitude")).and_then(Value::as_f64),
            json.get("location").and_then(|l| l.get("longitude")).and_then(Value::as_f64),
        ),
    };
    GeoInfo {
        ip: json.get("ip").and_then(Value::as_str).map(str::to_string),
        accept_language: country.as_deref().map(accept_language_for_country),
        country,
        timezone,
        location: lat.zip(lon).map(|(lat, lon)| format!("{lat},{lon}")),
    }
}

fn geo_from_ipinfo(json: &Value) -> GeoInfo {
    let country = json.get("country").and_then(Value::as_str).map(str::to_string);
    let location = json
        .get("loc")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            json.get("latitude")
                .and_then(Value::as_f64)
                .zip(json.get("longitude").and_then(Value::as_f64))
                .map(|(lat, lon)| format!("{lat},{lon}"))
        });
    GeoInfo {
        ip: json.get("ip").and_then(Value::as_str).map(str::to_string),
        accept_language: country.as_deref().map(accept_language_for_country),
        country,
        timezone: json.get("timezone").and_then(Value::as_str).map(str::to_string),
        location,
    }
}

async fn lookup_online(proxy: Option<&ProxySpec>, timeout_ms: u64) -> Option<GeoInfo> {
    let client = build_client(proxy, timeout_ms)?;
    if let Ok(response) = client.get("https://ipwho.is/").send().await {
        if let Ok(json) = response.json::<Value>().await {
            let geo = geo_from_ipwho(&json);
            if geo.is_usable() {
                return Some(geo);
            }
        }
    }
    if let Ok(response) = client.get("https://ipinfo.io/json").send().await {
        if let Ok(json) = response.json::<Value>().await {
            let geo = geo_from_ipinfo(&json);
            if geo.is_usable() {
                return Some(geo);
            }
        }
    }
    if ipapi_enabled() {
        let url = "http://ip-api.com/json/?fields=status,message,countryCode,timezone,lat,lon,query";
        if let Ok(response) = client.get(url).send().await {
            if let Ok(json) = response.json::<Value>().await {
                if json.get("status").and_then(Value::as_str) == Some("success") {
                    let country = json.get("countryCode").and_then(Value::as_str).map(str::to_string);
                    let location = json
                        .get("lat")
                        .and_then(Value::as_f64)
                        .zip(json.get("lon").and_then(Value::as_f64))
                        .map(|(lat, lon)| format!("{lat},{lon}"));
                    return Some(GeoInfo {
                        ip: json.get("query").and_then(Value::as_str).map(str::to_string),
                        accept_language: country.as_deref().map(accept_language_for_country),
                        country,
                        timezone: json.get("timezone").and_then(Value::as_str).map(str::to_string),
                        location,
                    });
                }
            }
        }
    }
    None
}

/// Resolve the egress geo **through `proxy`** within the configured budget. Never fails the launch:
/// `None` means the caller falls back to a locale-coherent timezone.
pub async fn resolve_geo(proxy: Option<&ProxySpec>) -> Option<GeoInfo> {
    if !geo_enabled() {
        return None;
    }
    let budget = geoip_timeout_ms();
    let future = async {
        // Make sure the exit IP exists even when the geolocation service fails, so the WebRTC IP
        // still matches the proxy.
        let ip = exit_ip(proxy, budget.min(8_000)).await;
        let mut geo = lookup_online(proxy, budget).await.unwrap_or_default();
        if geo.ip.is_none() {
            geo.ip = ip;
        }
        geo
    };
    tokio::time::timeout(Duration::from_millis(budget), future)
        .await
        .ok()
        .filter(GeoInfo::is_usable)
}

/// The proxies configured for this machine, in order: `NOVAMAIL_PROXIES` (one per line or
/// comma-separated) then `NOVAMAIL_PROXY`. Used to hand each new browser its own egress.
pub fn configured_proxies() -> Vec<ProxySpec> {
    let mut raw: Vec<String> = Vec::new();
    if let Ok(list) = std::env::var("NOVAMAIL_PROXIES") {
        raw.extend(
            list.split(['\n', '\r', ',', ';'])
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(str::to_string),
        );
    }
    if let Ok(single) = std::env::var("NOVAMAIL_PROXY") {
        let single = single.trim();
        if !single.is_empty() {
            raw.push(single.to_string());
        }
    }
    let mut proxies: Vec<ProxySpec> = Vec::new();
    for entry in raw {
        if let Some(proxy) = ProxySpec::parse(&entry) {
            if !proxies.contains(&proxy) {
                proxies.push(proxy);
            }
        }
    }
    proxies
}

/// The bypass list Chromium should skip the proxy for.
pub fn proxy_bypass() -> Option<String> {
    std::env::var("NOVAMAIL_PROXY_BYPASS")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proxy_urls() {
        let proxy = ProxySpec::parse("socks5://user:p%40ss@1.2.3.4:1080").unwrap();
        assert_eq!(proxy.scheme, "socks5");
        assert_eq!(proxy.server, "socks5://1.2.3.4:1080");
        assert_eq!(proxy.username, "user");
        assert_eq!(proxy.password, "p@ss");
        assert!(proxy.reqwest_url().starts_with("socks5h://user:p%40ss@1.2.3.4:1080"));
        assert!(ProxySpec::parse("1.2.3.4:8080").unwrap().server.starts_with("http://"));
        assert!(ProxySpec::parse("ftp://1.2.3.4:21").is_none());
    }

    #[test]
    fn proxy_engine_args_carry_credentials_separately() {
        let proxy = ProxySpec::parse("http://u:p@host:8080").unwrap();
        let args = proxy.engine_args(None);
        assert!(args.contains(&"--proxy-server=http://host:8080".to_string()));
        assert!(args.contains(&"--proxy-auth=u:p".to_string()));
        assert!(args.contains(&"--disable-quic".to_string()));
    }

    #[test]
    fn expands_one_locale_tag_like_chrome() {
        assert_eq!(resolve_languages("de-AT"), ("de-DE,de,en-US,en".to_string(), Some("de".to_string())));
        assert_eq!(resolve_languages("en-US"), ("en-US,en".to_string(), Some("en-US".to_string())));
        // A list is kept verbatim.
        assert_eq!(resolve_languages("de-AT,de,en"), ("de-AT,de,en".to_string(), Some("de".to_string())));
    }

    #[test]
    fn timezone_follows_locale() {
        assert_eq!(default_timezone("de-AT"), "Europe/Vienna");
        assert_eq!(default_timezone("en-US"), "America/New_York");
        assert_eq!(default_timezone("is-IS"), "America/New_York");
    }
}
