'use strict';

/**
 * Launcher-crash survival test (Tauri).
 *
 * Starts the real launcher in headless self-test "survival" mode, which spawns
 * N independent apps and then prints their PIDs. This script then HARD-kills
 * the launcher (without its child tree) and checks that every app is still
 * running. That is the requirement: a launcher crash must not take down healthy
 * apps.
 *
 * Usage:
 *   node scripts/test-launcher-crash-tauri.cjs [--apps 3]
 */

const { spawn, execFileSync } = require('node:child_process');
const path = require('node:path');
const fs = require('node:fs');

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
  console.error('[launcher-crash-test] novamail.exe not found. Run "cargo build" in src-tauri first.');
  process.exit(1);
}

const appCount = String(Math.max(2, Number(readArg('apps', '3')) || 3));

function isAlive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

function hardKill(pid, withTree = false) {
  if (!pid) return;
  if (process.platform === 'win32') {
    try {
      execFileSync('taskkill', ['/PID', String(pid), ...(withTree ? ['/T'] : []), '/F'], { stdio: 'ignore' });
      return;
    } catch {
      // fall through
    }
  }
  try { process.kill(pid, 'SIGKILL'); } catch { /* already gone */ }
}

console.log(`[launcher-crash-test] starting launcher with ${appCount} apps…`);

const launcher = spawn(binary, [], {
  cwd: root,
  env: {
    ...process.env,
    NOVAMAIL_SELFTEST: '1',
    NOVAMAIL_SELFTEST_MODE: 'survival',
    NOVAMAIL_SELFTEST_APPS: appCount,
    NOVAMAIL_WINDOW_HIDDEN: '1',
  },
  stdio: ['ignore', 'pipe', 'pipe'],
});

let buffer = '';
let resolvePids;
const pidsReady = new Promise((resolve) => { resolvePids = resolve; });

function consume(chunk) {
  const text = chunk.toString();
  process.stdout.write(text);
  buffer += text;
  const match = buffer.match(/\[selftest-pids\] (\{.*\})/);
  if (match) {
    try {
      resolvePids(JSON.parse(match[1]).pids || []);
    } catch {
      resolvePids([]);
    }
  }
}

launcher.stdout.on('data', consume);
launcher.stderr.on('data', consume);

(async () => {
  const appPids = await Promise.race([
    pidsReady,
    new Promise((_resolve, reject) => setTimeout(() => reject(new Error('timed out waiting for app PIDs')), 120_000)),
  ]);

  if (!appPids.length || appPids.some((pid) => !isAlive(pid))) {
    throw new Error(`apps were not all running before the crash: ${JSON.stringify(appPids)}`);
  }
  console.log(`[launcher-crash-test] ${appPids.length} apps running — now crashing the launcher (no tree kill)`);

  hardKill(launcher.pid, false);

  let survivors = [];
  for (let tick = 0; tick < 6; tick += 1) {
    await new Promise((resolve) => setTimeout(resolve, 1000));
    survivors = appPids.filter(isAlive);
    console.log(`[launcher-crash-test] t+${tick + 1}s: ${survivors.length}/${appPids.length} apps alive`);
  }

  const passed = survivors.length === appPids.length;

  console.log(`[launcher-crash-test] apps alive after launcher crash: ${survivors.length}/${appPids.length}`);

  for (const pid of appPids) hardKill(pid, true);
  await new Promise((resolve) => setTimeout(resolve, 500));

  const leaked = appPids.filter(isAlive);
  if (leaked.length) {
    console.error(`[launcher-crash-test] cleanup left processes alive: ${leaked.join(', ')}`);
  }

  console.log(`[launcher-crash-test] ${passed ? 'PASS' : 'FAIL'}`);
  process.exit(passed && !leaked.length ? 0 : 1);
})().catch((error) => {
  console.error(`[launcher-crash-test] ${error?.message || error}`);
  hardKill(launcher.pid, true);
  process.exit(1);
});
