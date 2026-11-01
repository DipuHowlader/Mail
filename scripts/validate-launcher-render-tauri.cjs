'use strict';

/**
 * Launcher render check (Tauri).
 *
 * Boots the real launcher window against the built renderer and verifies:
 *   - the window was created and the page loaded,
 *   - the launcher title is correct,
 *   - the runtime script was injected.
 *
 * Usage:
 *   npx vite build && node scripts/validate-launcher-render-tauri.cjs
 */

const { spawn } = require('node:child_process');
const path = require('node:path');
const fs = require('node:fs');

const root = path.resolve(__dirname, '..');

if (!fs.existsSync(path.join(root, 'dist', 'index.html'))) {
  console.error('[launcher-render] dist/index.html is missing. Run "npx vite build" first.');
  process.exit(1);
}

function findTauriBinary() {
  const candidates = [
    path.join(root, 'src-tauri', 'target', 'debug', 'novamail.exe'),
    path.join(root, 'src-tauri', 'target', 'release', 'novamail.exe'),
  ];
  for (const candidate of candidates) {
    if (fs.existsSync(candidate)) return candidate;
  }
  return null;
}

const binary = findTauriBinary();
if (!binary) {
  console.error('[launcher-render] novamail.exe not found. Run "cargo build" in src-tauri first.');
  process.exit(1);
}

const timeoutMs = 120_000;

console.log('[launcher-render] booting launcher window…');

const child = spawn(binary, [], {
  cwd: root,
  env: {
    ...process.env,
    NOVAMAIL_SELFTEST: '1',
    NOVAMAIL_SELFTEST_MODE: 'render',
    NOVAMAIL_WINDOW_HIDDEN: '1',
  },
  stdio: ['ignore', 'pipe', 'pipe'],
});

let buffer = '';
let report = null;

const timer = setTimeout(() => {
  console.error(`[launcher-render] timed out after ${timeoutMs}ms`);
  try { child.kill('SIGKILL'); } catch { /* gone */ }
  process.exit(1);
}, timeoutMs);

function consume(chunk) {
  const text = chunk.toString();
  process.stdout.write(text);
  buffer += text;
  const match = buffer.match(/\[selftest\] (\{.*\})/);
  if (match && !report) {
    try { report = JSON.parse(match[1]); } catch { report = null; }
  }
}

child.stdout.on('data', consume);
child.stderr.on('data', consume);

child.on('exit', () => {
  clearTimeout(timer);
  if (report) {
    console.log('\n[launcher-render] summary:');
    for (const step of report.steps || []) console.log(`  ✓ ${step}`);
    if (report.error) console.error(`  ✗ ${report.error}`);
    console.log(`  result: ${report.passed ? 'PASS' : 'FAIL'}`);
    process.exit(report.passed ? 0 : 1);
  }
  console.error('[launcher-render] no report produced');
  process.exit(1);
});
