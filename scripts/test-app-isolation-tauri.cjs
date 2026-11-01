'use strict';

/**
 * Fault-isolation test (Tauri).
 *
 * Launches the real Novamail launcher in headless self-test mode. That mode:
 *   1. creates N independent app processes,
 *   2. waits until every app reports ready (its own window + user-data dir),
 *   3. hard-kills one app process (SIGKILL / TerminateProcess),
 *   4. checks the other apps stay ready and keep heartbeating,
 *   5. restarts only the killed app and checks the rest are untouched.
 *
 * Exit code 0 means fault isolation held. Non-zero means it did not.
 *
 * Usage:
 *   node scripts/test-app-isolation-tauri.cjs [--apps 5]
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

function readArg(name, fallback) {
  const index = process.argv.indexOf(`--${name}`);
  if (index !== -1 && process.argv[index + 1]) return process.argv[index + 1];
  return fallback;
}

const binary = findTauriBinary();
if (!binary) {
  console.error('[isolation-test] novamail.exe not found. Run "cargo build" in src-tauri first.');
  process.exit(1);
}

const appCount = String(Math.max(2, Number(readArg('apps', '5')) || 5));
const timeoutMs = Number(readArg('timeout', '240000')) || 240_000;

console.log(`[isolation-test] starting Novamail launcher self-test with ${appCount} apps…`);

const child = spawn(binary, [], {
  cwd: root,
  env: {
    ...process.env,
    NOVAMAIL_SELFTEST: '1',
    NOVAMAIL_SELFTEST_MODE: 'isolation',
    NOVAMAIL_SELFTEST_APPS: appCount,
    NOVAMAIL_WINDOW_HIDDEN: '1',
  },
  stdio: ['ignore', 'pipe', 'pipe'],
});

let buffer = '';
let report = null;
let exitCode = 1;

const timer = setTimeout(() => {
  console.error(`[isolation-test] timed out after ${timeoutMs}ms`);
  try { child.kill('SIGKILL'); } catch { /* already gone */ }
  finish(1);
}, timeoutMs);

function finish(code) {
  clearTimeout(timer);
  try {
    fs.rmSync(path.join(os.tmpdir(), 'novamail-selftest'), { recursive: true, force: true });
  } catch { /* best effort */ }
  process.exit(code);
}

function consume(chunk) {
  const text = chunk.toString();
  process.stdout.write(text);
  buffer += text;
  const match = buffer.match(/\[selftest\] (\{.*\})/);
  if (match && !report) {
    try {
      report = JSON.parse(match[1]);
    } catch {
      report = null;
    }
  }
}

child.stdout.on('data', consume);
child.stderr.on('data', consume);

child.on('exit', (code) => {
  if (report) {
    exitCode = report.passed ? 0 : 1;
    console.log('\n[isolation-test] summary:');
    for (const step of report.steps || []) console.log(`  ✓ ${step}`);
    if (report.error) console.error(`  ✗ ${report.error}`);
    console.log(`  apps: ${report.appCount} · result: ${report.passed ? 'PASS' : 'FAIL'}`);
  } else {
    console.error(`[isolation-test] no report produced (launcher exit code ${code})`);
    exitCode = 1;
  }
  finish(exitCode);
});
