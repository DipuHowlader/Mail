# Device emulation & fingerprint rotation

Novamail drives Gmail inside the bundled Clearcote Chromium (engine `150.0.7871.114`). Every browser
instance opened from **Add profile** presents a **complete new machine identity** — a different OS,
screen, DPR, core/memory count, touch capability and GPU — drawn from a bundled catalogue of **76
real-world devices**. The identity is applied as one coherent unit so that the User-Agent, Client
Hints, `navigator.platform`, screen metrics, `devicePixelRatio`, WebGL strings and pointer media
queries never contradict each other, and the session leaves no tell that it is automated.

The implementation lives in the Rust backend:

| Concern | File |
| --- | --- |
| Device catalogue (data) | [`src-tauri/resources/device-catalog.json`](../src-tauri/resources/device-catalog.json) |
| Catalogue generator | [`scripts/build-device-catalog.cjs`](../scripts/build-device-catalog.cjs) |
| Device / persona resolution | [`src-tauri/src/persona.rs`](../src-tauri/src/persona.rs) |
| Egress geo, proxy plumbing, locale | [`src-tauri/src/geo.rs`](../src-tauri/src/geo.rs) |
| Human input model | [`src-tauri/src/browser/humanize.rs`](../src-tauri/src/browser/humanize.rs) |
| CDP dispatch + launch driver | [`src-tauri/src/browser/chromium.rs`](../src-tauri/src/browser/chromium.rs) |
| Launch integration | [`src-tauri/src/automation/mod.rs`](../src-tauri/src/automation/mod.rs) (`create_profile`, ~line 540) |

## Device catalogue

[`src-tauri/resources/device-catalog.json`](../src-tauri/resources/device-catalog.json) holds **76
coherent real-world machines**: 19 Windows, 38 macOS and 19 Linux profiles. Each
entry is one internally consistent machine:

```json
{
  "id": "win-dell-xps-13-9310",
  "os": "windows",
  "osName": "Windows",
  "model": "Dell XPS 13 9310",
  "uaPlatform": "Windows",
  "navigatorPlatform": "Win32",
  "mobile": false,
  "brand": "chrome",
  "screen": { "width": 1920, "height": 1200, "availWidth": 1920, "availHeight": 1152 },
  "devicePixelRatio": 1.25,
  "colorDepth": 24,
  "hardwareConcurrency": 8,
  "deviceMemory": 8,
  "maxTouchPoints": 0,
  "gpuVendor": "Google Inc. (Intel)",
  "gpuRenderer": "ANGLE (Intel, Intel(R) Iris(R) Xe Graphics (0x00009A49) Direct3D11 vs_5_0 ps_5_0, D3D11)"
}
```

The file is embedded into the binary with `include_str!`, so it is read at build time and can be
extended without touching Rust code — regenerate it with:

```bash
node scripts/build-device-catalog.cjs   # rewrites src-tauri/resources/device-catalog.json
```

Then rebuild the backend. Edit the pools inside that script (screens, GPUs, and hardware models) to change
which machines rotate.

## How a launch resolves an identity

`persona::resolve(&options)` runs once per **Add profile** and returns a [`Persona`](../src-tauri/src/persona.rs)
carrying the engine switches, the window geometry, and a [`PersonaSpec`](../src-tauri/src/persona.rs)
for telemetry/UI. The steps:

1. **Seed.** A fresh `novamail-<uuid>` seed per launch (override with `NOVAMAIL_DEVICE_SEED`, or per
   call via `options.deviceSeed`). The seed roots both the device draw and the human-motion persona,
   so a re-launch of the same seed reproduces the same machine *and* the same hand.
2. **Pool.** The set of OS families the draw may use (default **`host`** — see
   [Choosing a pool](#choosing-a-pool)).
3. **GPU filter.** Optional `NOVAMAIL_DEVICE_GPU_CLASS` narrows to one vendor class within the pool.
4. **Draw.** Candidates are filtered to the pool, then to the GPU class, then one is selected by
   `fnv1a(seed) % candidates.len()`. An explicit `NOVAMAIL_DEVICE_ID` (or `options.deviceId`) pins a
   catalogue entry by id and wins over the draw.
5. **Proxy + geo.** A proxy is resolved (explicit `options.proxy` → `NOVAMAIL_PROXIES` /
   `NOVAMAIL_PROXY`, chosen deterministically from the seed), and the egress is geolocated *through*
   that proxy (see [IP-based timezone and locale](#ip-based-timezone-and-locale)).
6. **Engine switches.** [`engine_args()`](../src-tauri/src/persona.rs) turns the device + geo into
   the command line; each switch is emitted **exactly once**.

The driver is handed the same seed (`config.humanize(Some(persona.spec.seed))`) so the motor persona
matches the fingerprint, and `--window-size` is set through `DriverConfig::window_size` from
`Persona::window` (`availWidth × availHeight`) rather than being duplicated in the flag list.

## Engine-level persona

The identity is applied entirely through the vendored engine's own switches — **no anti-detection
JavaScript is injected**, because a page-visible script hook is itself a tell. The switches, one per
facet:

| Switch | Source | Effect |
| --- | --- | --- |
| `--fingerprint=<seed>` | launch seed | Roots canvas/audio farbling |
| `--fingerprint-platform=<os>` | catalogue `os` | Routes UA, UA-CH, `navigator.platform`, pointer media queries and plugins to that OS |
| `--fingerprint-brand=<brand>` | catalogue `brand` | Chrome branding in UA / UA-CH |
| `--fingerprint-gpu-vendor` / `-gpu-renderer` | catalogue GPU | WebGL vendor/renderer strings |
| `--fingerprint-hardware-concurrency` / `-device-memory` | catalogue | `navigator.hardwareConcurrency` / `deviceMemory` |
| `--fingerprint-screen-width` / `-screen-height` | catalogue | `screen.width` / `screen.height` |
| `--fingerprint-avail-width` / `-avail-height` | catalogue | `screen.availWidth` / `availHeight` |
| `--fingerprint-color-depth` | catalogue | `screen.colorDepth` |
| `--fingerprint-device-pixel-ratio` | catalogue | `window.devicePixelRatio` |
| `--fingerprint-max-touch-points` | catalogue | `navigator.maxTouchPoints`; `0` for desktops |
| `--fingerprint-storage-quota=<MB>` | `NOVAMAIL_STORAGE_QUOTA_MB` | Realistic `storage.estimate().quota` instead of the 10 GiB incognito cap |
| `--accept-lang=<list>` | English | HTTP `Accept-Language` and `navigator.languages` |
| `--lang=<ui>` | English | Chromium UI locale |
| `--timezone=<IANA>` | egress geo | The clock, pinned so the host's UTC never leaks |
| `--fingerprint-location=<lat,lon>` | egress geo | Geolocation coordinates |
| `--webrtc-ip=<ip>` | egress geo | Fabricates the WebRTC `srflx` candidate at the proxy IP; no real STUN is sent |
| `--disable-features=BrowsingTopics,…` | constant | Disables the Privacy Sandbox / intrusive-ad APIs a real stealth build would not answer |

Because the engine rewrites a single `blink::UserAgentMetadata`, the UA, `Sec-CH-UA-*` headers and
`navigator.userAgentData` can never disagree.

**The browser language is English by default.** The egress locale only drives `--accept-lang` /
`--lang` when a proxy is configured (where matching the exit region is the point); on a direct
connection the browser stays English even though the clock, coordinates and WebRTC IP still follow
the host's own egress. `NOVAMAIL_ACCEPT_LANG` pins a specific tag and overrides both cases.

**One `--disable-features`, always.** Chromium keeps only the last `--disable-features` on the
command line, so the Privacy Sandbox list is the only such flag emitted for the automation session —
[`merge_baseline_args`](../src-tauri/src/browser/chromium.rs) unions the baseline's `TranslateUI`
with the persona list into a single switch.

**No duplicates.** Every flag above appears once. A repeated flag is a shape no real browser
produces, and the engine exposes its command line over CDP, so `merge_baseline_args` collapses every
switch by *name* before launch: a later value (the persona's `--lang`) overrides the baseline's, and
the feature lists are merged. Without this, chromiumoxide's name-keyed `HashMap` would emit both
`--lang=en-US` and `--lang=en_US` and let Chromium pick a **random** one on each launch.

## No automation tells

### The launch baseline

The driver is built on `chromiumoxide`, whose `BrowserConfig` injects a fixed `DEFAULT_ARGS` list —
and that list contains **`--enable-automation`**, the switch that raises Chrome's *"Chrome is being
controlled by automated test software"* infobar. `--disable-infobars` does **not** hide it in modern
Chrome, so the only reliable fix is to not pass the flag.

[`ChromiumDriver::launch`](../src-tauri/src/browser/chromium.rs) therefore calls
`.disable_default_args()` and supplies its own [`BASELINE_ARGS`](../src-tauri/src/browser/chromium.rs)
— chromiumoxide's defaults **minus** `--enable-automation` and `--enable-blink-features=IdleDetection`
(an API that is off by default on a real browser). Every caller's persona switches are added after the
baseline and then collapsed by switch name: the caller's value wins for scalars (so the persona's
`--lang`/`--disable-features` are the effective ones) and the feature lists are unioned.

> **Maintainer note.** If the browser ever shows the automation infobar again, check chromiumoxide's
> `DEFAULT_ARGS` in a dependency bump — `disable_default_args()` is what keeps it out.

### The inspected surface

The driver connects to the engine over the DevTools protocol on a private port, but the automation
surface a page inspects is stripped:

- `--enable-automation` is never passed (see above),
- `navigator.webdriver` is `false`,
- no `playwright` / `puppeteer` / `cdc_` / `__driver` / `webdriver` globals reach the page,
- `window.chrome` is present as on a real Chrome.

The CDP connection drives input and navigation, but the protocol chatter is kept off the page: the
humanised pointer path issues **one CDP round-trip per sample** and sleeps *locally*
(`tokio::time::sleep`), rather than a `waitForTimeout`-style call per step, which would itself emit a
round-trip per pause.

## IP-based timezone and locale

A persona is only coherent if its **clock, language, coordinates and WebRTC IP agree with the address
the sites actually see** — the proxy's exit IP, not this machine's. Every lookup therefore goes
*through the proxy*, never around it. The behaviour mirrors Clearcote's own `geoip` directive:

1. **Exit IP** — a small IP-echo reached through the proxy: `https://api.ipify.org`, then
   `https://icanhazip.com` (or `CLEARCOTE_GEOIP_ECHO_URL`, comma-separated).
2. **Lookup** — `https://ipwho.is/`, then `https://ipinfo.io/json`, and optionally
   `http://ip-api.com/json/...` when `CLEARCOTE_GEOIP_IPAPI` is `1`/`true`/`yes`/`on`. The exit IP is
   looked up *as that provider reports it when reached through the proxy*, so the answer is already
   the exit's region.
3. **Derive** — the ISO country maps to one OS locale (`accept_language_for_country`), which is
   expanded to Chrome's own language list (`de-AT` → `de-DE,de,en-US,en`, UI `de`) and paired with a
   locale-coherent IANA timezone (`default_timezone`).

The whole lookup is bounded by `NOVAMAIL_GEO_TIMEOUT_MS` (default `8000`; overridden by
`CLEARCOTE_GEOIP_TIMEOUT_SECONDS` when set). **A failure never blocks a launch**: if nothing usable
resolves, the persona falls back to a locale-coherent default (`en-US` / `America/New_York`), and the
exit IP gathered in step 1 still pins `--webrtc-ip` so WebRTC keeps matching the proxy.

The **language** is English by default in every case — proxy or not — so the operator
always gets an English browser. `NOVAMAIL_ACCEPT_LANG` pins a specific tag when the network
identity must match a proxy's exit region; the clock, coordinates and WebRTC IP still follow
the egress in every case.

`NOVAMAIL_GEOIP=0` disables the layer entirely.

## Proxy plumbing

A proxy can be supplied per launch via `options.proxy` (a string, or `{server, username, password}`),
or configured for the machine with `NOVAMAIL_PROXIES` (one per line / comma-separated) and
`NOVAMAIL_PROXY`. When a list is configured, each launch picks one deterministically from the seed so
a re-launch keeps its egress. `ProxySpec` accepts `http`, `https`, `socks5`, `socks5h`, `socks4` and
`socks4a`; a bare `host:port` is treated as `http`.

Credentials are **never** placed in `--proxy-server` (Chromium's proxy parser drops a URL that carries
userinfo). They travel on the engine's own authentication flags:

| Switch | When |
| --- | --- |
| `--proxy-server=<scheme://host:port>` | always |
| `--socks5-credentials=<user:pass>` | SOCKS proxies |
| `--proxy-auth=<user:pass>` | HTTP(S) proxies |
| `--proxy-bypass-list=<list>` | when `NOVAMAIL_PROXY_BYPASS` is set |
| `--disable-quic` | always — a SOCKS/HTTP proxy carries TCP only |

## Interaction realism

Human input is modelled in [`humanize.rs`](../src-tauri/src/browser/humanize.rs), a Rust port of
Clearcote's `motion`/`humanize` model, and dispatched over CDP by
[`chromium.rs`](../src-tauri/src/browser/chromium.rs):

- **Minimum-jerk submovements** — a reach is a primary ballistic submovement plus one or two
  corrective ones (Meyer's optimised-submovement model), each following `s(τ)=10τ³−15τ⁴+6τ⁵`, so the
  velocity peaks mid-path and decelerates into the target.
- **Fitts's-law duration**, not duration proportional to distance.
- **Coloured noise** — an Ornstein–Uhlenbeck drift plus an 8–12 Hz tremor and correlated sub-pixel
  jitter, never flat white gaussian noise (whose flat spectrum is itself a tell).
- **A seeded motor persona** — speed, tremor, overshoot, handedness and cadence consistent within a
  session and unlinkable across seeds.
- **Human dwell times** — a mousedown held 60–150 ms, a key held 45–120 ms, inter-key flight,
  thinking pauses, and fast pairs that *roll over* (the next key goes down before the last comes up).

Two deliberate choices:

- **Fat-finger typos are not simulated.** A mail sender must never corrupt an address, and the
  correction path has to read the field back to know what to delete — a correctness bug is worse than
  the marginal realism. Everything else a keystroke-dynamics collector measures (hold, flight,
  rollover, thinking pauses, Shift handling) is reproduced.
- **Long text is inserted atomically.** Text over `HUMAN_TYPE_MAX_CHARS` (200) uses
  `Input.insertText` rather than key-by-key, so a 2 kB body does not turn a bulk send into a
  standstill; short fields are typed key-by-key. Characters no physical US-layout key produces
  (composed/zero-width/emoji/non-Latin) are batched into one atomic insert.

Where each lane is live:

- **Typing is live.** The driver's `insert_text` plans a keystroke sequence with the persona hand and
  dispatches it key-by-key over CDP (`Input.dispatchKeyEvent` — the exact, production-proven path the
  shortcuts already take) whenever humanization is enabled for the launch, the text is short, and it
  is recipient data *only* when the value is neither a bare address nor a comma-separated list.
  After dispatch it verifies the focused element holds the text; on any miss it clears the field and
  replays the proven atomic `Input.insertText`/DOM path, so a humanised attempt can never corrupt a
  value. Recipient data always takes the atomic path, for two reasons typing key-by-key cannot fix:
  a bare address would burn seconds of dwell/flight pauses per message on a value the compose flow
  has always committed reliably in one protocol insert, and a comma-separated list must stay atomic
  anyway because peoplekit chips the To field on a *keyed* comma — a key-driven comma would commit
  the first address mid-insert and duplicate the rest into a second token. Subjects and bodies keep
  the keystroke realism; a body that merely quotes an address among other words is not address-shaped
  and stays typed.
- **The pointer warm-up is live.** Before the first keyboard interaction of a session
  (`press_key`/`insert_text`), the driver dispatches one ambient pointer burst — mouse *moves* only —
  so a collector never sees a keystroke as the session's first input event. Pure `MouseMoved` events
  are safe on this engine; the risk was only ever the pressed-click path.
- **The pointer press path stays a ready building block.** The Gmail send flow presses compose/send
  through the DOM because the CDP `Input.dispatchMouseEvent` *press* path proved unstable on some
  Gmail layouts — this did not change. Every synthetic DOM pointer event the flow does dispatch now
  carries a jittered, element-anchored point (`clientX/Y`, `pageX/Y`, `screenX/Y`), because an event
  left at the default fires at screen origin (0,0) — the shape of a hand that only ever presses the
  top-left corner.

### Self-verification

`NOVAMAIL_SELFTEST_MODE=typing NOVAMAIL_WINDOW_HIDDEN=1 novamail.exe` drives the bundled engine
through the real humanized driver against a local page and verifies what a behavioural collector
sees: the typed text arrives as a real keydown stream spelling the text (with Shift pairing for the
uppercase characters) paced over seconds rather than one atomic round-trip, the warm-up pointer
burst precedes the session's first keystroke, `press_key` inserts its key, recipient data (a bare
address and a comma-separated list) deliberately bypasses typing (zero keydowns) while the value
still lands, and `navigator.webdriver` is not `true`. It prints one `[selftest] {json}` line and
exits non-zero on a failure.

## Choosing a pool

`NOVAMAIL_DEVICE_POOL` is a comma-separated list. Tokens:

| Token | Expands to |
| --- | --- |
| `host` / `auto` / `native` | this machine's OS family |
| `desktop` | `windows,macos,linux` |
| `all` | `windows,macos,linux` |
| an OS name | `windows` \| `macos` \| `linux` |

An unknown token is ignored; an empty result falls back to the host OS.

The **default is `host`**, so each launch presents the host OS family — the most conservative
configuration, where only the hardware identity changes between instances and the canvas/font
hashes stay native. This avoids the cross-OS fingerprint mismatch that Google's sign-in risk
engine (and other detectors) flag. Set `NOVAMAIL_DEVICE_POOL=all` (or `desktop`/`mobile`) to
rotate across OS families when that trade-off is acceptable.

## Documented limitations

These are properties of presenting a device **persona** on a desktop Chromium engine. Each is a
deliberate trade-off, not an unfinished feature.

1. **Cross-OS font/canvas hashes.** On a non-host pool the font list and canvas text hashes stay
   host-native (e.g. Windows fonts on a Windows host) and will not match a claimed macOS/Linux
   OS. This is the cost of rotating across OS families; `NOVAMAIL_DEVICE_POOL=host` avoids it.
2. **GPU model string vs pixels.** The catalogue spoofs a GPU *model* while the real pixels come from
   the host GPU. `NOVAMAIL_DEVICE_GPU_CLASS` keeps the vendor *class* coherent; an exact-model
   mismatch is not checkable by the host.
3. **GPU rendering backend.** The GPU renders with the host backend while the persona spoofs the
   device string — the string-vs-render mismatch Clearcote documents for any persona on a desktop
   rasteriser.
4. **macOS persona on a Windows host** reports a Direct3D-family WebGL renderer that no real Mac has.
   The default `all` pool includes macOS profiles.

## Configuration reference

Everything is optional.

| Variable | Default | Purpose |
| --- | --- | --- |
| `NOVAMAIL_DEVICE_EMULATION` | on | `0`/`false`/`off`/`no` disables the persona layer entirely (engine pass-through; window still set to the device size) |
| `NOVAMAIL_DEVICE_POOL` | `host` | OS families to rotate across (see [Choosing a pool](#choosing-a-pool)) |
| `NOVAMAIL_DEVICE_SEED` | random per launch | Pin one fixed identity (device **and** hand) |
| `NOVAMAIL_MOBILE_FINGERPRINT` | — | Legacy alias for `NOVAMAIL_DEVICE_SEED` |
| `NOVAMAIL_DEVICE_ID` | — | Pin one catalogue entry by `id`, overriding the draw |
| `NOVAMAIL_DEVICE_GPU_CLASS` | none | `intel` \| `amd` \| `nvidia` \| `apple`; filters the draw to that vendor class |
| `NOVAMAIL_STORAGE_QUOTA_MB` | `64000` | Reported `navigator.storage` quota |
| `NOVAMAIL_GEOIP` | on | `0` disables the egress-geo lookup |
| `NOVAMAIL_ACCEPT_LANG` | — | Pins the browser language (e.g. `en-US`); default is English, or the egress locale when a proxy is set |
| `NOVAMAIL_GEO_TIMEOUT_MS` | `8000` | Budget for the egress-geo lookup |
| `CLEARCOTE_GEOIP_TIMEOUT_SECONDS` | — | Overrides `NOVAMAIL_GEO_TIMEOUT_MS` when set |
| `CLEARCOTE_GEOIP_ECHO_URL` | ipify → icanhazip | Comma-separated IP-echo endpoints |
| `CLEARCOTE_GEOIP_IPAPI` | off | `1`/`true`/`yes`/`on` enables the cleartext `ip-api.com` fallback |
| `NOVAMAIL_PROXY` | — | A single proxy handed to each instance |
| `NOVAMAIL_PROXIES` | — | A list of proxies (one per line / comma-separated), drawn per seed |
| `NOVAMAIL_PROXY_BYPASS` | — | Hosts that skip the proxy (`--proxy-bypass-list`) |

The `CLEARCOTE_GEOIP_*` variables are honoured so a machine already configured for Clearcote behaves
the same here.

> **Note on `ip-api.com`.** The optional fallback is cleartext HTTP and licensed for non-commercial
> use only. The HTTPS providers (`ipwho.is`, `ipinfo.io`) cover the same case without those
> restrictions, so the fallback is off unless explicitly enabled.
