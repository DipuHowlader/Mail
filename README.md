# Novamail

A premium, multi-account desktop email client built with Tauri, React, Vite, Tailwind CSS, and Clearcote Chromium-based Gmail automation. License activation → connect one or more Google accounts → compose and send.

## Stack

- **Renderer:** React 18 + Vite + Tailwind (dark, Arc/Linear-inspired UI)
- **Shell:** Tauri (Rust), with isolated app processes
- **Mail:** Gmail compose and send through isolated Clearcote Chromium sessions
- **Auth:** The user signs in directly with Google inside the Clearcote Chromium browser; Novamail never collects the Google password
- **Storage:** a shared locked JSON config store persists application-owned campaign data and account identities; each app instance uses a separate temporary browser profile for its Gmail session

### Mail merge architecture

Campaigns are the durable source of truth. Recipient lists, templates, message fields,
attachments, and merge variables are stored as application data and autosaved while composing.
Each browser profile, debugging connection, and automation run belongs to its own instance. Closing
an instance removes its temporary browser session without deleting campaigns.
- **Recipient import:** CSV and XLSX files, read and parsed by the desktop process

## Getting started

```bash
npm install
cp .env.example .env
```

Set LICENSE_SERVER_URL in .env to the license backend verification endpoint.

No browser install is required. Gmail automation runs in the Clearcote Chromium build that
ships inside NovaMail: the installer contains the browser, so the app never downloads or
installs anything at runtime and never uses a system browser. The browser is vendored under
`vendor/clearcote/browser` and copied into the packaged app by the Tauri build; `npm run
build` fetches it once on the build machine (via `npm run prepare:browser`) if it is not
already present. 

Run in development:

```bash
npm run dev:tauri
```

The Vite renderer uses port `5173`. The separate license admin dashboard is configured
to use port `5174`, so both development servers can run at the same time without competing
for the Tauri dev server port.

Build the renderer only (e.g. to sanity-check the UI):

```bash
npm run build
```

Build a distributable (via Tauri):

```bash
npm run build:tauri
```

## Device emulation & fingerprint rotation

The interactive Gmail browser presents a **complete new machine identity on every launch**, so
accounts are not linked by a shared fingerprint. It uses a Clearcote **engine-level persona** plus a
**device catalog** (bundled with the Tauri app, 76 real-world machines: 19 Windows, 38 macOS and 19
Linux profiles) that sets
the OS, screen size, DPR, cores, memory, GPU and touch capability as one coherent unit — matching the
UA and Client Hints, `navigator.platform`, pointer/touch media queries, `plugins` and the window size,
across JavaScript and HTTP.

The **default pool rotates across all three OS families** (`windows`, `macos`, `linux`), so
each launch behaves like a different machine — the behavior the "different OS every launch"
requirement asks for. Set `NOVAMAIL_DEVICE_POOL=host` for the most conservative configuration, where
the only thing that changes between instances is the hardware identity and the fonts/canvas metrics
stay native to this machine. `NOVAMAIL_DEVICE_GPU_CLASS` can filter the draw to one GPU vendor class
so a spoofed GPU string stays consistent with the pixels the machine renders. The browser also takes
the **timezone, language and geolocation of the network's public IP** (`NOVAMAIL_PROXY` /
`NOVAMAIL_PROXIES`) and human-like, motion-based input.

The egress IP is discovered through the proxy, then geolocated (`ipwho.is`, then `ipinfo.io`); the
resolved timezone/`Accept-Language`/coordinates/WebRTC IP are pinned explicitly so a temporary lookup
failure never blocks sign-in. The engine also removes every automation tell: `navigator.webdriver` is
false, no Playwright/CDP globals are leaked into pages, and `window.chrome` is present. See
[`docs/device-emulation.md`](./docs/device-emulation.md) for the full design, the environment
variables, and the documented residual tells.

## What's real vs. what you still need to supply

The app includes isolated Clearcote Chromium-based Gmail sending, CSV/XLSX parsing, and server-backed license verification. You need to provide a real licensing backend for centrally managed, revocable keys.

## Recipient imports

Imported CSV/XLSX recipients are reviewed and added to **To**. The message is not sent automatically:
the user must review the list and press **Send Email**. The compose page also supports loading an
`.html`/`.htm` file as the email body template, which remains editable before sending.

The subject is a saved, editable template. It can be loaded from a reviewed `.txt` or `.md` file
(the first non-empty line is used), then personalized with the same recipient variables as the
message body, such as `$email`, `$username`, `$company_name`, or imported column headings.
NovaMail deliberately does not rotate subject lines automatically.

## License backend

The standalone backend and admin dashboard are in [`license-backend/`](./license-backend/).
License keys bind permanently to their first device and cannot be reactivated after revocation.
Keys expire; MongoDB TTL cleanup is asynchronous, and the API checks expiration on every request.
Keys and device identifiers are HMACed at rest. The desktop client stores the key encrypted with
the operating system's secure storage and revalidates it at startup.

```bash
cd license-backend
npm install
copy .env.example .env
# Set MONGODB_URI, ADMIN_API_TOKEN, JWT_SECRET, LICENSE_KEY_PEPPER,
# DEVICE_BINDING_PEPPER, ADMIN_ORIGINS, and the initial admin credentials
npm start
```

Use [`admin/AdminDashboard.jsx`](./license-backend/admin/AdminDashboard.jsx) in a private React/Vite admin app.
Install `lucide-react` in that admin app and render it with `apiBaseUrl` and `adminToken` props. Do not
ship the admin token inside the desktop app or a public client bundle.

## Project structure

```
src-tauri/                  # Rust backend (Tauri)
  src/
    automation/             # Gmail automation: profiles, mail sender, gmail driver
    browser/                # Clearcote Chromium driver, humanize motion model, paths
    persona.rs              # device/OS persona resolution (reads the bundled catalog)
    geo.rs                  # egress-IP geolocation, proxy plumbing, locale resolution
    license.rs              # license verification and secure storage
    store.rs, files.rs      # config store and file access
    documents.rs, template.rs, launcher.rs, runtime.rs, selftest.rs
    commands.rs, control.rs, lib.rs, main.rs
  resources/
    device-catalog.json     # 76 profiles: 19 Windows, 38 macOS, 19 Linux
docs/
  device-emulation.md       # device emulation design, rotation, env vars and limitations
scripts/
  build-device-catalog.cjs  # regenerates src-tauri/resources/device-catalog.json (deterministic)
  prepare-browser.cjs       # fetches the vendored Clearcote Chromium on the build machine
  *-tauri.cjs               # benchmarks and end-to-end checks
src/                        # React renderer
  components/               # TitleBar, LicenseGate, RecipientImport, Toast, ...
  pages/                    # ComposerPage, TagsPage
  context/AppContext.jsx    # stage machine, accounts, toasts
  services/                 # ipc.js / tauriBridge.js — thin wrappers over the Tauri commands
  launcher/LauncherApp.jsx  # multi-instance launcher UI
  utils/                    # validation/formatting helpers
```

