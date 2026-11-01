'use strict';

/**
 * Make sure the Clearcote Chromium browser is vendored before packaging.
 *
 * NovaMail ships its own browser so an installed copy never downloads or installs
 * anything: the browser is copied into `vendor/clearcote/browser` and the Tauri build
 * places it in `<resources>/clearcote` (see the `extraResources` entry in package.json).
 *
 * `vendor/clearcote` is intentionally not committed to git (it is ~660 MB), so a fresh
 * checkout will not have it. This script fetches it ONCE on the build machine and copies
 * the verified tree into `vendor/clearcote`. When the folder is already present (the
 * normal case on a machine that has built before) this is a no-op.
 */

const fs = require('node:fs');
const path = require('node:path');

const projectRoot = path.resolve(__dirname, '..');
const vendorRoot = path.join(projectRoot, 'vendor', 'clearcote');
const browserDir = path.join(vendorRoot, 'browser');
const binaryName = process.platform === 'win32' ? 'chrome.exe' : 'chrome';
const executable = path.join(browserDir, binaryName);

function log(message) {
  console.log(`[build:browser] ${message}`);
}

// UI locales the app can ever ask the engine to use (see `CHROME_UI_LOCALES` and
// the `chrome_ui_locale` mapping in `src-tauri/src/geo.rs`). Keep this list in
// sync with that file: the persona pins `--lang` from the egress country, and the
// engine needs the matching `<locale>.pak` for its own menus and dialogs.
const UI_LOCALES = [
  'af', 'am', 'ar', 'bg', 'bn', 'ca', 'cs', 'da', 'de', 'el', 'en-GB', 'en-US', 'es', 'es-419',
  'et', 'fa', 'fi', 'fil', 'fr', 'gu', 'he', 'hi', 'hr', 'hu', 'id', 'it', 'ja', 'kn', 'ko',
  'lt', 'lv', 'ml', 'mr', 'ms', 'nb', 'nl', 'pl', 'pt-BR', 'pt-PT', 'ro', 'ru', 'sk', 'sl', 'sr',
  'sv', 'sw', 'ta', 'te', 'th', 'tr', 'uk', 'ur', 'vi', 'zh-CN', 'zh-TW',
];

// Files that ship with the engine but are only used by its own developers.
// `VkICD_mock_icd.dll` is the Vulkan debugger's mock implementation loader
// (`vk_swiftshader_icd.json` and nothing else in the tree references an ICD,
// and the loader only picks a mock ICD up through an explicit env var), so the
// installer never needs to ship it.
const DEV_ONLY_FILES = [
  'VkLayer_khronos_validation.dll',
  'VkICD_mock_icd.dll',
  'debug.log',
];

function formatMb(bytes) {
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

/**
 * Drop everything the app can never use: locale packs for languages the persona
 * cannot request (plus their `.pak.info` update metadata), the Vulkan validation
 * layer, and the engine's stray debug log. This only touches the engine's own UI
 * resources — it never changes `navigator.language`, `Accept-Language`, the
 * timezone, or any other fingerprint signal, which the persona sets through
 * launch flags.
 */
function pruneBrowser(dir) {
  const keep = new Set(UI_LOCALES);
  let removed = 0;
  let freed = 0;

  const localesDir = path.join(dir, 'locales');
  if (fs.existsSync(localesDir)) {
    for (const entry of fs.readdirSync(localesDir, { withFileTypes: true })) {
      if (!entry.isFile()) continue;
      const name = entry.name;
      // `.pak.info` is only used by the engine's own differential-update system,
      // never at runtime, so it is always safe to drop.
      const isInfo = name.endsWith('.pak.info');
      const match = name.match(/^(.+?)\.pak(?:\.info)?$/);
      if (!match) continue;
      const base = match[1].replace(/_(?:FEMININE|MASCULINE)$/, '');
      if (!isInfo && keep.has(base)) continue;
      const full = path.join(localesDir, name);
      try {
        freed += fs.statSync(full).size;
        fs.rmSync(full, { force: true });
        removed += 1;
      } catch {
        // A locked file is not worth failing the build over.
      }
    }
  }

  for (const name of DEV_ONLY_FILES) {
    const full = path.join(dir, name);
    try {
      if (fs.existsSync(full)) {
        freed += fs.statSync(full).size;
        fs.rmSync(full, { force: true });
        removed += 1;
      }
    } catch {
      // Best effort.
    }
  }

  if (removed) log(`Trimmed ${removed} unused engine files (${formatMb(freed)}).`);
  return freed;
}

function findBinary(dir) {
  const stack = [dir];
  while (stack.length) {
    const current = stack.pop();
    let entries;
    try {
      entries = fs.readdirSync(current, { withFileTypes: true });
    } catch {
      continue;
    }
    for (const entry of entries) {
      const full = path.join(current, entry.name);
      if (entry.isDirectory()) stack.push(full);
      else if (entry.name.toLowerCase() === binaryName.toLowerCase()) return full;
    }
  }
  return null;
}

async function main() {
  if (fs.existsSync(executable)) {
    log(`Bundled browser already present: ${path.relative(projectRoot, executable)}`);
    pruneBrowser(browserDir);
    return;
  }

  log('Bundled browser is missing. Fetching it once at build time.');
  log('This is a one-time build step. The installer will ship the browser, so end users download nothing.');

  let download;
  try {
    ({ download } = await import('clearcote'));
  } catch (error) {
    throw new Error(
      `Could not load the Clearcote browser downloader. Run "npm install" in the project folder and try again.\n${error.message}`,
    );
  }

  let downloadedExecutable;
  try {
    downloadedExecutable = await download({ quiet: false });
  } catch (error) {
    throw new Error(
      'Could not fetch the browser required to build NovaMail. Connect to the internet once, then build again.\n' +
        `Underlying error: ${error.message}`,
    );
  }

  const sourceDir = path.dirname(downloadedExecutable);
  log(`Copying the downloaded browser into ${path.relative(projectRoot, browserDir)} ...`);
  fs.mkdirSync(vendorRoot, { recursive: true });
  // Replace whatever is there so a partial or stale tree is never packaged.
  fs.rmSync(browserDir, { recursive: true, force: true });
  fs.cpSync(sourceDir, browserDir, { recursive: true });

  if (!fs.existsSync(executable)) {
    // The archive layout put the binary in a sub-folder; flatten just the binary's
    // own directory so the vendored tree keeps the expected flat shape.
    const nested = findBinary(browserDir);
    if (!nested) {
      throw new Error(
        `The browser was downloaded but ${binaryName} was not found. Delete vendor/clearcote and build again.`,
      );
    }
    fs.rmSync(browserDir, { recursive: true, force: true });
    fs.cpSync(path.dirname(nested), browserDir, { recursive: true });
  }

  if (!fs.existsSync(executable)) {
    throw new Error(`The bundled browser is still incomplete at ${executable}. Delete vendor/clearcote and build again.`);
  }
  pruneBrowser(browserDir);
  log(`Bundled browser ready: ${path.relative(projectRoot, executable)}`);
}

main().catch((error) => {
  console.error(`[build:browser] ${error.message}`);
  process.exitCode = 1;
});
