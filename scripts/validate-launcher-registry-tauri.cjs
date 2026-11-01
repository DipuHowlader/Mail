'use strict';

/**
 * App-list persistence check (Tauri).
 *
 * Boots the launcher in registry self-test mode (which writes to a throwaway
 * registry file) and verifies that apps round-trip through disk, that a leftover
 * running process is reported as "orphaned", and that removing an app persists.
 *
 * Usage:
 *   node scripts/validate-launcher-registry-tauri.cjs
 */

const { spawn } = require('node:child_process');
const path = require('node:path');
const fs = require('node:fs');
const os = require('node:os');

const root = path.resolve(__dirname, '..');

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
  console.error('[launcher-registry] novamail.exe not found. Run "cargo build" in src-tauri first.');
  process.exit(1);
}

const registryPath = path.join(os.tmpdir(), `novamail-registry-test-${process.pid}.json`);

try { fs.rmSync(registryPath, { force: true }); } catch { /* ignore */ }

console.log('[launcher-registry] checking app-list persistence…');

const child = spawn(binary, [], {
  cwd: root,
  env: {
    ...process.env,
    NOVAMAIL_SELFTEST: '1',
    NOVAMAIL_SELFTEST_MODE: 'registry',
    NOVAMAIL_WINDOW_HIDDEN: '1',
    NOVAMAIL_REGISTRY_PATH: registryPath,
  },
  stdio: ['ignore', 'pipe', 'pipe'],
});

let buffer = '';
let report = null;
const timeoutMs = 90_000;

const timer = setTimeout(() => {
  console.error(`[launcher-registry] timed out after ${timeoutMs}ms`);
  try { child.kill('SIGKILL'); } catch { /* gone */ }
  cleanup(1);
}, timeoutMs);

function cleanup(code) {
  clearTimeout(timer);
  try { fs.rmSync(registryPath, { force: true }); } catch { /* ignore */ }
  process.exit(code);
}

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
  if (report) {
    console.log('\n[launcher-registry] summary:');
    for (const step of report.steps || []) console.log(`  ✓ ${step}`);
    if (report.error) console.error(`  ✗ ${report.error}`);
    console.log(`  result: ${report.passed ? 'PASS' : 'FAIL'}`);
    cleanup(report.passed ? 0 : 1);
    return;
  }
  console.error('[launcher-registry] no report produced');
  cleanup(1);
});
