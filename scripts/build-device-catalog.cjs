'use strict';

/**
 * Generate the bundled device catalog: `src-tauri/resources/device-catalog.json`.
 *
 * NovaMail presents a *different, coherent machine* on every browser launch so accounts are
 * never linked by a shared hardware fingerprint. This script holds the curated catalogue of
 * real-world machines (76 across Windows, macOS and Linux) and normalises each row
 * into the shape `src-tauri/src/persona.rs` consumes:
 *
 *   - the four screen numbers a page can read (`screen.width/height`, `availWidth/availHeight`),
 *     with the OS' own taskbar/menu-bar inset applied to the height,
 *   - `devicePixelRatio` / `colorDepth` (a DPR is part of the machine, not a random draw),
 *   - `hardwareConcurrency` / `deviceMemory` (Chrome caps `deviceMemory` at 8),
 *   - `maxTouchPoints` (0 for a mouse desktop, 5–10 for a touch laptop),
 *   - the *coherent* GPU pair: an UNMASKED vendor and a renderer whose dialect matches the OS
 *     (Direct3D 11 on Windows, Metal on macOS, Mesa/OpenGL on Linux).
 *
 * The values are chosen to stay mutually consistent: a 4K panel gets a GPU that can drive one,
 * a 4-core laptop is not paired with 64 GB of RAM, and so on. Re-run with:
 *
 *   node scripts/build-device-catalog.cjs
 *
 * The Rust side (persona.rs) reads the emitted JSON at compile time; it never talks to this
 * script at runtime. `NOVAMAIL_DEVICE_POOL` selects which operating systems are eligible.
 */

const fs = require('node:fs');
const path = require('node:path');

/** How much of the screen the OS chrome (taskbar/menu bar) takes from `availHeight`. */
const OS_INSET = { windows: 48, macos: 25, linux: 27 };

/** `navigator.platform` a genuine browser reports for each OS. */
const NAVIGATOR_PLATFORM = {
  windows: 'Win32',
  macos: 'MacIntel',
  linux: 'Linux x86_64',
};

/** The UA-CH platform token the engine uses for each OS. */
const UA_PLATFORM = {
  windows: 'Windows',
  macos: 'macOS',
  linux: 'Linux',
};

/** Human-readable OS name, for telemetry/UI only (the engine derives the real UA version). */
const OS_NAME = {
  windows: 'Windows',
  macos: 'macOS',
  linux: 'Linux',
};

/**
 * The catalogue. One row per real machine archetype:
 *   [id, os, model, screenW, screenH, dpr, cores, memoryGB, touchPoints, gpuVendor, gpuRenderer]
 *
 * Memory is capped at 8 GB on purpose: `navigator.deviceMemory` reports at most 8 in Chrome, so
 * claiming 16/32 would be a value no real browser produces. `cores` are the real core counts.
 */
const ROWS = [
  // ── Windows ────────────────────────────────────────────────────────────────────────────────
  ['win-dell-xps-13-9310', 'windows', 'Dell XPS 13 9310', 1920, 1200, 1.25, 8, 8, 0, 'Google Inc. (Intel)', 'ANGLE (Intel, Intel(R) Iris(R) Xe Graphics (0x00009A49) Direct3D11 vs_5_0 ps_5_0, D3D11)'],
  ['win-dell-xps-15-9520', 'windows', 'Dell XPS 15 9520', 1920, 1200, 1.25, 14, 8, 0, 'Google Inc. (Intel)', 'ANGLE (Intel, Intel(R) Iris(R) Xe Graphics (0x000046A6) Direct3D11 vs_5_0 ps_5_0, D3D11)'],
  ['win-dell-latitude-5420', 'windows', 'Dell Latitude 5420', 1366, 768, 1, 4, 4, 0, 'Google Inc. (Intel)', 'ANGLE (Intel, Intel(R) UHD Graphics (0x00009A60) Direct3D11 vs_5_0 ps_5_0, D3D11)'],
  ['win-dell-precision-5570', 'windows', 'Dell Precision 5570', 1920, 1080, 1, 16, 8, 0, 'Google Inc. (NVIDIA)', 'ANGLE (NVIDIA, NVIDIA T1200 Laptop GPU Direct3D11 vs_5_0 ps_5_0, D3D11)'],
  ['win-hp-spectre-x360-14', 'windows', 'HP Spectre x360 14', 1920, 1280, 1.25, 8, 8, 10, 'Google Inc. (Intel)', 'ANGLE (Intel, Intel(R) Iris(R) Xe Graphics (0x00009A49) Direct3D11 vs_5_0 ps_5_0, D3D11)'],
  ['win-hp-pavilion-15', 'windows', 'HP Pavilion 15', 1920, 1080, 1, 8, 8, 0, 'Google Inc. (AMD)', 'ANGLE (AMD, AMD Radeon(TM) Graphics (0x00001638) Direct3D11 vs_5_0 ps_5_0, D3D11)'],
  ['win-hp-elitebook-840-g8', 'windows', 'HP EliteBook 840 G8', 1920, 1200, 1.25, 8, 8, 0, 'Google Inc. (Intel)', 'ANGLE (Intel, Intel(R) Iris(R) Xe Graphics (0x00009A49) Direct3D11 vs_5_0 ps_5_0, D3D11)'],
  ['win-lenovo-thinkpad-x1-carbon-g9', 'windows', 'Lenovo ThinkPad X1 Carbon Gen 9', 1920, 1200, 1.5, 8, 8, 0, 'Google Inc. (Intel)', 'ANGLE (Intel, Intel(R) Iris(R) Xe Graphics (0x00009A49) Direct3D11 vs_5_0 ps_5_0, D3D11)'],
  ['win-lenovo-ideapad-5', 'windows', 'Lenovo IdeaPad 5', 1920, 1080, 1.25, 8, 8, 0, 'Google Inc. (AMD)', 'ANGLE (AMD, AMD Radeon(TM) Vega 8 Graphics (0x000015D8) Direct3D11 vs_5_0 ps_5_0, D3D11)'],
  ['win-lenovo-legion-5-pro', 'windows', 'Lenovo Legion 5 Pro', 2560, 1600, 1.5, 16, 8, 0, 'Google Inc. (NVIDIA)', 'ANGLE (NVIDIA, NVIDIA GeForce RTX 3060 Laptop GPU Direct3D11 vs_5_0 ps_5_0, D3D11)'],
  ['win-lenovo-thinkcentre-m70q', 'windows', 'Lenovo ThinkCentre M70q', 1920, 1080, 1, 8, 8, 0, 'Google Inc. (Intel)', 'ANGLE (Intel, Intel(R) UHD Graphics 630 (0x00003E92) Direct3D11 vs_5_0 ps_5_0, D3D11)'],
  ['win-asus-rog-zephyrus-g14', 'windows', 'ASUS ROG Zephyrus G14', 2560, 1600, 1.5, 16, 8, 0, 'Google Inc. (NVIDIA)', 'ANGLE (NVIDIA, NVIDIA GeForce RTX 4060 Laptop GPU Direct3D11 vs_5_0 ps_5_0, D3D11)'],
  ['win-acer-aspire-5', 'windows', 'Acer Aspire 5', 1920, 1080, 1, 4, 4, 0, 'Google Inc. (Intel)', 'ANGLE (Intel, Intel(R) UHD Graphics (0x00009A60) Direct3D11 vs_5_0 ps_5_0, D3D11)'],
  ['win-msi-gf63-thin', 'windows', 'MSI GF63 Thin', 1920, 1080, 1, 8, 8, 0, 'Google Inc. (NVIDIA)', 'ANGLE (NVIDIA, NVIDIA GeForce GTX 1650 Direct3D11 vs_5_0 ps_5_0, D3D11)'],
  ['win-surface-laptop-4', 'windows', 'Microsoft Surface Laptop 4', 1536, 1024, 1.5, 8, 8, 10, 'Google Inc. (AMD)', 'ANGLE (AMD, AMD Radeon(TM) Graphics (0x00001638) Direct3D11 vs_5_0 ps_5_0, D3D11)'],
  ['win-custom-desktop-gtx-1660s', 'windows', 'Custom Desktop (GTX 1660 SUPER)', 1920, 1080, 1, 6, 8, 0, 'Google Inc. (NVIDIA)', 'ANGLE (NVIDIA, NVIDIA GeForce GTX 1660 SUPER Direct3D11 vs_5_0 ps_5_0, D3D11)'],
  ['win-custom-desktop-rtx-3060', 'windows', 'Custom Desktop (RTX 3060)', 2560, 1440, 1, 12, 8, 0, 'Google Inc. (NVIDIA)', 'ANGLE (NVIDIA, NVIDIA GeForce RTX 3060 Direct3D11 vs_5_0 ps_5_0, D3D11)'],
  ['win-custom-desktop-rtx-4070', 'windows', 'Custom Desktop (RTX 4070)', 3840, 2160, 1.5, 16, 8, 0, 'Google Inc. (NVIDIA)', 'ANGLE (NVIDIA, NVIDIA GeForce RTX 4070 Direct3D11 vs_5_0 ps_5_0, D3D11)'],
  ['win-custom-desktop-rx-580', 'windows', 'Custom Desktop (RX 580)', 1920, 1080, 1, 6, 8, 0, 'Google Inc. (AMD)', 'ANGLE (AMD, AMD Radeon RX 580 Direct3D11 vs_5_0 ps_5_0, D3D11)'],

  // ── macOS ──────────────────────────────────────────────────────────────────────────────────
  ['mac-macbook-air-m1-13', 'macos', 'MacBook Air (M1, 2020)', 1440, 900, 2, 8, 8, 0, 'Google Inc. (Apple)', 'ANGLE (Apple, ANGLE Metal Renderer: Apple M1, Unspecified Version)'],
  ['mac-macbook-air-m2-13', 'macos', 'MacBook Air (M2, 2022)', 1470, 956, 2, 8, 8, 0, 'Google Inc. (Apple)', 'ANGLE (Apple, ANGLE Metal Renderer: Apple M2, Unspecified Version)'],
  ['mac-macbook-air-m2-15', 'macos', 'MacBook Air 15 (M2, 2023)', 1710, 1107, 2, 8, 8, 0, 'Google Inc. (Apple)', 'ANGLE (Apple, ANGLE Metal Renderer: Apple M2, Unspecified Version)'],
  ['mac-macbook-air-m3-13', 'macos', 'MacBook Air (M3, 2024)', 1470, 956, 2, 8, 8, 0, 'Google Inc. (Apple)', 'ANGLE (Apple, ANGLE Metal Renderer: Apple M3, Unspecified Version)'],
  ['mac-macbook-pro-13-m2', 'macos', 'MacBook Pro 13 (M2, 2022)', 1280, 800, 2, 8, 8, 0, 'Google Inc. (Apple)', 'ANGLE (Apple, ANGLE Metal Renderer: Apple M2, Unspecified Version)'],
  ['mac-macbook-pro-14-m1-pro', 'macos', 'MacBook Pro 14 (M1 Pro, 2021)', 1512, 982, 2, 10, 8, 0, 'Google Inc. (Apple)', 'ANGLE (Apple, ANGLE Metal Renderer: Apple M1 Pro, Unspecified Version)'],
  ['mac-macbook-pro-14-m3', 'macos', 'MacBook Pro 14 (M3, 2023)', 1512, 982, 2, 12, 8, 0, 'Google Inc. (Apple)', 'ANGLE (Apple, ANGLE Metal Renderer: Apple M3, Unspecified Version)'],
  ['mac-macbook-pro-16-m1-max', 'macos', 'MacBook Pro 16 (M1 Max, 2021)', 1728, 1117, 2, 10, 8, 0, 'Google Inc. (Apple)', 'ANGLE (Apple, ANGLE Metal Renderer: Apple M1 Max, Unspecified Version)'],
  ['mac-macbook-pro-16-m2-max', 'macos', 'MacBook Pro 16 (M2 Max, 2023)', 1728, 1117, 2, 12, 8, 0, 'Google Inc. (Apple)', 'ANGLE (Apple, ANGLE Metal Renderer: Apple M2 Max, Unspecified Version)'],
  ['mac-macbook-pro-16-m3-pro', 'macos', 'MacBook Pro 16 (M3 Pro, 2023)', 1728, 1117, 2, 12, 8, 0, 'Google Inc. (Apple)', 'ANGLE (Apple, ANGLE Metal Renderer: Apple M3 Pro, Unspecified Version)'],
  ['mac-macbook-pro-15-2019', 'macos', 'MacBook Pro 15 (2019, Intel)', 1440, 900, 2, 8, 8, 0, 'Google Inc. (Intel)', 'ANGLE (Intel Inc., Intel(R) Iris(R) Plus Graphics 655, OpenGL 4.1)'],
  ['mac-macbook-air-2020-intel', 'macos', 'MacBook Air (2020, Intel)', 1440, 900, 2, 4, 8, 0, 'Google Inc. (Intel)', 'ANGLE (Intel Inc., Intel(R) Iris(R) Plus Graphics, OpenGL 4.1)'],
  ['mac-imac-24-m1', 'macos', 'iMac 24 (M1, 2021)', 2240, 1260, 2, 8, 8, 0, 'Google Inc. (Apple)', 'ANGLE (Apple, ANGLE Metal Renderer: Apple M1, Unspecified Version)'],
  ['mac-imac-27-2020', 'macos', 'iMac 27 (2020, Intel)', 2560, 1440, 2, 8, 8, 0, 'Google Inc. (AMD)', 'ANGLE (AMD, AMD Radeon Pro 5300 OpenGL Engine, OpenGL 4.1)'],
  ['mac-imac-pro-2017', 'macos', 'iMac Pro (2017)', 2560, 1440, 2, 8, 8, 0, 'Google Inc. (AMD)', 'ANGLE (AMD, AMD Radeon Pro Vega 56 OpenGL Engine, OpenGL 4.1)'],
  ['mac-mac-mini-m1', 'macos', 'Mac mini (M1, 2020)', 1920, 1080, 2, 8, 8, 0, 'Google Inc. (Apple)', 'ANGLE (Apple, ANGLE Metal Renderer: Apple M1, Unspecified Version)'],
  ['mac-mac-mini-m2', 'macos', 'Mac mini (M2, 2023)', 1920, 1080, 2, 8, 8, 0, 'Google Inc. (Apple)', 'ANGLE (Apple, ANGLE Metal Renderer: Apple M2, Unspecified Version)'],
  ['mac-mac-studio-m1-max', 'macos', 'Mac Studio (M1 Max, 2022)', 2560, 1440, 2, 10, 8, 0, 'Google Inc. (Apple)', 'ANGLE (Apple, ANGLE Metal Renderer: Apple M1 Max, Unspecified Version)'],
  ['mac-mac-studio-m2-ultra', 'macos', 'Mac Studio (M2 Ultra, 2023)', 3840, 2160, 2, 24, 8, 0, 'Google Inc. (Apple)', 'ANGLE (Apple, ANGLE Metal Renderer: Apple M2 Ultra, Unspecified Version)'],
  ['mac-macbook-air-11-2015', 'macos', 'MacBook Air 11 (Early 2015)', 1366, 768, 1, 4, 4, 0, 'Google Inc. (Intel)', 'ANGLE (Intel Inc., Intel(R) HD Graphics 6000 OpenGL Engine, OpenGL 4.1)'],
  ['mac-macbook-air-13-2017', 'macos', 'MacBook Air 13 (2017)', 1440, 900, 1, 4, 8, 0, 'Google Inc. (Intel)', 'ANGLE (Intel Inc., Intel(R) HD Graphics 6000 OpenGL Engine, OpenGL 4.1)'],
  ['mac-macbook-pro-13-retina-2015', 'macos', 'MacBook Pro 13 Retina (Early 2015)', 1440, 900, 2, 4, 8, 0, 'Google Inc. (Intel)', 'ANGLE (Intel Inc., Intel(R) Iris(TM) Graphics 6100 OpenGL Engine, OpenGL 4.1)'],
  ['mac-macbook-pro-15-retina-2015', 'macos', 'MacBook Pro 15 Retina (Mid 2015)', 1440, 900, 2, 8, 8, 0, 'Google Inc. (AMD)', 'ANGLE (AMD, AMD Radeon R9 M370X OpenGL Engine, OpenGL 4.1)'],
  ['mac-macbook-pro-13-intel-2020', 'macos', 'MacBook Pro 13 (2020, Intel)', 1440, 900, 2, 8, 8, 0, 'Google Inc. (Intel)', 'ANGLE (Intel Inc., Intel(R) Iris(TM) Plus Graphics OpenGL Engine, OpenGL 4.1)'],
  ['mac-macbook-pro-16-intel-2019', 'macos', 'MacBook Pro 16 (2019, Intel)', 1792, 1120, 2, 16, 8, 0, 'Google Inc. (AMD)', 'ANGLE (AMD, AMD Radeon Pro 5500M OpenGL Engine, OpenGL 4.1)'],
  ['mac-macbook-pro-13-m1-2020', 'macos', 'MacBook Pro 13 (M1, 2020)', 1440, 900, 2, 8, 8, 0, 'Google Inc. (Apple)', 'ANGLE (Apple, ANGLE Metal Renderer: Apple M1, Unspecified Version)'],
  ['mac-macbook-pro-14-m2-pro-2023', 'macos', 'MacBook Pro 14 (M2 Pro, 2023)', 1512, 982, 2, 12, 8, 0, 'Google Inc. (Apple)', 'ANGLE (Apple, ANGLE Metal Renderer: Apple M2 Pro, Unspecified Version)'],
  ['mac-macbook-pro-14-m3-max-2023', 'macos', 'MacBook Pro 14 (M3 Max, 2023)', 1512, 982, 2, 16, 8, 0, 'Google Inc. (Apple)', 'ANGLE (Apple, ANGLE Metal Renderer: Apple M3 Max, Unspecified Version)'],
  ['mac-macbook-pro-16-m3-max-2023', 'macos', 'MacBook Pro 16 (M3 Max, 2023)', 1728, 1117, 2, 16, 8, 0, 'Google Inc. (Apple)', 'ANGLE (Apple, ANGLE Metal Renderer: Apple M3 Max, Unspecified Version)'],
  ['mac-macbook-pro-14-m4-pro-2024', 'macos', 'MacBook Pro 14 (M4 Pro, 2024)', 1512, 982, 2, 14, 8, 0, 'Google Inc. (Apple)', 'ANGLE (Apple, ANGLE Metal Renderer: Apple M4 Pro, Unspecified Version)'],
  ['mac-macbook-air-13-m3-2024', 'macos', 'MacBook Air 13 (M3, 2024)', 1470, 956, 2, 8, 8, 0, 'Google Inc. (Apple)', 'ANGLE (Apple, ANGLE Metal Renderer: Apple M3, Unspecified Version)'],
  ['mac-macbook-air-15-m3-2024', 'macos', 'MacBook Air 15 (M3, 2024)', 1710, 1107, 2, 8, 8, 0, 'Google Inc. (Apple)', 'ANGLE (Apple, ANGLE Metal Renderer: Apple M3, Unspecified Version)'],
  ['mac-mac-mini-intel-2018', 'macos', 'Mac mini (2018, Intel)', 1920, 1080, 1, 12, 8, 0, 'Google Inc. (Intel)', 'ANGLE (Intel Inc., Intel(R) UHD Graphics 630 OpenGL Engine, OpenGL 4.1)'],
  ['mac-mac-mini-m4-2024', 'macos', 'Mac mini (M4, 2024)', 2560, 1440, 1, 10, 8, 0, 'Google Inc. (Apple)', 'ANGLE (Apple, ANGLE Metal Renderer: Apple M4, Unspecified Version)'],
  ['mac-imac-24-m3-2023', 'macos', 'iMac 24 (M3, 2023)', 2240, 1260, 2, 8, 8, 0, 'Google Inc. (Apple)', 'ANGLE (Apple, ANGLE Metal Renderer: Apple M3, Unspecified Version)'],
  ['mac-imac-27-intel-2019', 'macos', 'iMac 27 (2019, Intel)', 2560, 1440, 2, 16, 8, 0, 'Google Inc. (AMD)', 'ANGLE (AMD, AMD Radeon Pro 570X OpenGL Engine, OpenGL 4.1)'],
  ['mac-mac-studio-m2-max-2023', 'macos', 'Mac Studio (M2 Max, 2023)', 3840, 2160, 2, 12, 8, 0, 'Google Inc. (Apple)', 'ANGLE (Apple, ANGLE Metal Renderer: Apple M2 Max, Unspecified Version)'],
  ['mac-mac-pro-intel-2019', 'macos', 'Mac Pro (2019, Intel)', 2560, 1440, 1, 24, 8, 0, 'Google Inc. (AMD)', 'ANGLE (AMD, AMD Radeon Pro 580X OpenGL Engine, OpenGL 4.1)'],

  // ── Linux ──────────────────────────────────────────────────────────────────────────────────
  ['linux-ubuntu-thinkpad-t480', 'linux', 'Ubuntu — ThinkPad T480', 1920, 1080, 1, 8, 8, 0, 'Google Inc. (Intel)', 'ANGLE (Intel, Mesa Intel(R) UHD Graphics 620 (KBL GT2), OpenGL 4.6 (Core Profile) Mesa 23.0.4-0ubuntu1~22.04.1)'],
  ['linux-ubuntu-thinkpad-t14-amd', 'linux', 'Ubuntu — ThinkPad T14 (AMD)', 1920, 1200, 1, 8, 8, 0, 'Google Inc. (AMD)', 'ANGLE (AMD, AMD Radeon Vega 7 (renoir, LLVM 15.0.6, DRM 3.47, 6.2.0-26-generic), OpenGL 4.6 (Core Profile) Mesa 23.0.4-0ubuntu1~22.04.1)'],
  ['linux-ubuntu-dell-xps-13-9370', 'linux', 'Ubuntu — Dell XPS 13 9370', 1920, 1080, 1, 8, 8, 0, 'Google Inc. (Intel)', 'ANGLE (Intel, Mesa Intel(R) UHD Graphics 620 (KBL GT2), OpenGL 4.6 (Core Profile) Mesa 23.0.4-0ubuntu1~22.04.1)'],
  ['linux-ubuntu-lenovo-legion-5', 'linux', 'Ubuntu — Lenovo Legion 5', 1920, 1080, 1, 16, 8, 0, 'Google Inc. (NVIDIA)', 'ANGLE (NVIDIA, NVIDIA GeForce RTX 3060/PCIe/SSE2, OpenGL 4.6.0 NVIDIA 535.113.01)'],
  ['linux-fedora-thinkpad-x1c', 'linux', 'Fedora — ThinkPad X1 Carbon', 1920, 1080, 1, 8, 8, 0, 'Google Inc. (Intel)', 'ANGLE (Intel, Mesa Intel(R) Iris(R) Xe Graphics (TGL GT2), OpenGL 4.6 (Core Profile) Mesa 23.1.6)'],
  ['linux-fedora-xps-15', 'linux', 'Fedora — Dell XPS 15', 1920, 1200, 1, 12, 8, 0, 'Google Inc. (Intel)', 'ANGLE (Intel, Mesa Intel(R) Iris(R) Xe Graphics (TGL GT2), OpenGL 4.6 (Core Profile) Mesa 23.1.6)'],
  ['linux-fedora-quadro-p620', 'linux', 'Fedora — Workstation (Quadro P620)', 2560, 1440, 1, 8, 8, 0, 'Google Inc. (NVIDIA)', 'ANGLE (NVIDIA, NVIDIA Quadro P620/PCIe/SSE2, OpenGL 4.6.0 NVIDIA 535.113.01)'],
  ['linux-debian-optiplex-7050', 'linux', 'Debian — OptiPlex 7050', 1920, 1080, 1, 4, 4, 0, 'Google Inc. (Intel)', 'ANGLE (Intel, Mesa Intel(R) HD Graphics 630 (KBL GT2), OpenGL 4.6 (Core Profile) Mesa 22.3.6)'],
  ['linux-debian-thinkpad-p50', 'linux', 'Debian — ThinkPad P50', 1920, 1080, 1, 8, 8, 0, 'Google Inc. (NVIDIA)', 'ANGLE (NVIDIA, NVIDIA Quadro M2000M/PCIe/SSE2, OpenGL 4.6.0 NVIDIA 525.125.06)'],
  ['linux-arch-desktop-rtx-3070', 'linux', 'Arch — Desktop (RTX 3070)', 2560, 1440, 1, 12, 8, 0, 'Google Inc. (NVIDIA)', 'ANGLE (NVIDIA, NVIDIA GeForce RTX 3070/PCIe/SSE2, OpenGL 4.6.0 NVIDIA 535.113.01)'],
  ['linux-arch-desktop-rtx-4060', 'linux', 'Arch — Desktop (RTX 4060)', 2560, 1440, 1, 12, 8, 0, 'Google Inc. (NVIDIA)', 'ANGLE (NVIDIA, NVIDIA GeForce RTX 4060/PCIe/SSE2, OpenGL 4.6.0 NVIDIA 545.29.06)'],
  ['linux-arch-hidpi-4k', 'linux', 'Arch — HiDPI Desktop (4K)', 3840, 2160, 1.5, 16, 8, 0, 'Google Inc. (Intel)', 'ANGLE (Intel, Mesa Intel(R) Iris(R) Xe Graphics (TGL GT2), OpenGL 4.6 (Core Profile) Mesa 23.1.6)'],
  ['linux-pop-os-lemur-pro', 'linux', 'Pop!_OS — System76 Lemur Pro', 1920, 1080, 1, 8, 8, 0, 'Google Inc. (Intel)', 'ANGLE (Intel, Mesa Intel(R) Iris(R) Xe Graphics (TGL GT2), OpenGL 4.6 (Core Profile) Mesa 23.1.6)'],
  ['linux-manjaro-thinkpad-t14', 'linux', 'Manjaro — ThinkPad T14', 1920, 1200, 1, 8, 8, 0, 'Google Inc. (AMD)', 'ANGLE (AMD, AMD Radeon Vega 7 (renoir, LLVM 16.0.6, DRM 3.49, 6.4.10-1-MANJARO), OpenGL 4.6 (Core Profile) Mesa 23.1.6)'],
  ['linux-ubuntu-desktop-rx-6600', 'linux', 'Ubuntu — Desktop (RX 6600)', 1920, 1080, 1, 8, 8, 0, 'Google Inc. (AMD)', 'ANGLE (AMD, AMD Radeon RX 6600 (navi23, LLVM 15.0.7, DRM 3.49, 6.2.0-33-generic), OpenGL 4.6 (Core Profile) Mesa 23.0.4-0ubuntu1~22.04.1)'],
  ['linux-opensuse-desktop-rx-580', 'linux', 'openSUSE — Desktop (RX 580)', 1920, 1080, 1, 8, 8, 0, 'Google Inc. (AMD)', 'ANGLE (AMD, AMD Radeon RX 580 Series (polaris10, LLVM 16.0.6, DRM 3.54, 6.6.1-1-default), OpenGL 4.6 (Core Profile) Mesa 23.2.1)'],
  ['linux-ubuntu-framework-13', 'linux', 'Ubuntu — Framework Laptop 13', 2256, 1504, 1.5, 8, 8, 0, 'Google Inc. (Intel)', 'ANGLE (Intel, Mesa Intel(R) Iris(R) Xe Graphics (TGL GT2), OpenGL 4.6 (Core Profile) Mesa 23.1.6)'],
  ['linux-ubuntu-elitebook-840', 'linux', 'Ubuntu — HP EliteBook 840', 1920, 1080, 1, 8, 8, 0, 'Google Inc. (Intel)', 'ANGLE (Intel, Mesa Intel(R) UHD Graphics 620 (KBL GT2), OpenGL 4.6 (Core Profile) Mesa 23.0.4-0ubuntu1~22.04.1)'],
  ['linux-mint-latitude-5490', 'linux', 'Linux Mint — Dell Latitude 5490', 1366, 768, 1, 4, 4, 0, 'Google Inc. (Intel)', 'ANGLE (Intel, Mesa Intel(R) UHD Graphics 620 (KBL GT2), OpenGL 4.6 (Core Profile) Mesa 21.2.6)'],

];

function buildDevice(row) {
  const [id, os, model, width, height, dpr, hardwareConcurrency, deviceMemory, maxTouchPoints, gpuVendor, gpuRenderer] = row;
  const inset = OS_INSET[os] ?? 0;
  const availHeight = Math.max(400, height - inset);
  return {
    id,
    os,
    osName: OS_NAME[os],
    model,
    uaPlatform: UA_PLATFORM[os],
    navigatorPlatform: NAVIGATOR_PLATFORM[os],
    mobile: false,
    brand: 'chrome',
    screen: { width, height, availWidth: width, availHeight },
    devicePixelRatio: dpr,
    colorDepth: 24,
    hardwareConcurrency,
    deviceMemory,
    maxTouchPoints,
    gpuVendor,
    gpuRenderer,
  };
}

function main() {
  const devices = ROWS.map(buildDevice);
  const byOs = devices.reduce((acc, d) => ((acc[d.os] = (acc[d.os] || 0) + 1), acc), {});
  const catalog = {
    schema: 1,
    description:
      'Coherent real-world device catalogue for NovaMail browser personas. One identity is drawn per browser launch.',
    counts: byOs,
    devices,
  };
  const outPath = path.resolve(__dirname, '..', 'src-tauri', 'resources', 'device-catalog.json');
  fs.mkdirSync(path.dirname(outPath), { recursive: true });
  fs.writeFileSync(outPath, `${JSON.stringify(catalog, null, 2)}\n`);
  console.log(`[device-catalog] wrote ${devices.length} devices (${JSON.stringify(byOs)}) -> ${path.relative(process.cwd(), outPath)}`);
}

main();
