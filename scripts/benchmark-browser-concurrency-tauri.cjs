'use strict';

/**
 * Measure independent Chromium launches using the binary Tauri drives through
 * CDP. Build/package the app first so the bundled runtime is available.
 * Usage: node scripts/benchmark-browser-concurrency-tauri.cjs [1 2 3 4 5]
 */

const os = require('node:os');
const path = require('node:path');
const fs = require('node:fs/promises');
const { spawn, execFileSync } = require('node:child_process');

function executablePath() {
  if (process.env.NOVAMAIL_CHROMIUM_PATH) return process.env.NOVAMAIL_CHROMIUM_PATH;
  const filename = process.platform === 'win32' ? 'chrome.exe' : 'chrome';
  return path.resolve('vendor', 'clearcote', 'browser', filename);
}

function countChromeProcesses() {
  try {
    if (process.platform === 'win32') {
      const output = execFileSync('tasklist', ['/FI', 'IMAGENAME eq chrome.exe', '/FO', 'CSV', '/NH'], { encoding: 'utf8' });
      return output.split(/\r?\n/).filter((line) => /^"?chrome\.exe/i.test(line.trim())).length;
    }
    return Number(execFileSync('pgrep', ['-fc', 'chrome'], { encoding: 'utf8' }).trim()) || 0;
  } catch {
    return null;
  }
}

const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function launch(executable, profile, index) {
  const started = process.hrtime.bigint();
  const child = spawn(executable, [
    '--headless=new',
    '--no-first-run',
    '--no-default-browser-check',
    '--disable-background-timer-throttling',
    '--disable-backgrounding-occluded-windows',
    '--disable-renderer-backgrounding',
    '--disable-ipc-flooding-protection',
    '--remote-debugging-port=0',
    `--user-data-dir=${profile}`,
    'about:blank',
  ], { stdio: 'ignore', windowsHide: true });

  await new Promise((resolve, reject) => {
    child.once('spawn', resolve);
    child.once('error', reject);
  });
  const devtoolsFile = path.join(profile, 'DevToolsActivePort');
  const deadline = Date.now() + 30000;
  while (Date.now() < deadline) {
    if (child.exitCode !== null) throw new Error(`Chromium ${index} exited before DevTools became ready.`);
    try {
      await fs.access(devtoolsFile);
      return { child, launchMs: Number(process.hrtime.bigint() - started) / 1e6 };
    } catch {
      await delay(50);
    }
  }
  await close({ child });
  throw new Error(`Chromium ${index} did not open its DevTools endpoint within 30 seconds.`);
}

async function close(entry) {
  if (!entry || entry.child.exitCode !== null) return;
  let treeKillCompleted = false;
  if (process.platform === 'win32') {
    try {
      execFileSync('taskkill', ['/PID', String(entry.child.pid), '/T', '/F'], { stdio: 'ignore' });
      treeKillCompleted = true;
    } catch {
      // The direct child handle is still ours even when taskkill is restricted.
    }
  }
  if (!treeKillCompleted) {
    entry.child.kill('SIGTERM');
  }
  await Promise.race([
    new Promise((resolve) => entry.child.once('exit', resolve)),
    delay(5000),
  ]);
}

async function runLevel(level, executable) {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'novamail-tauri-browser-benchmark-'));
  const profiles = Array.from({ length: level }, (_, index) => path.join(root, `profile-${index}`));
  await Promise.all(profiles.map((profile) => fs.mkdir(profile, { recursive: true })));
  const before = countChromeProcesses();
  const started = process.hrtime.bigint();
  let entries = [];
  try {
    const launches = await Promise.allSettled(profiles.map((profile, index) => launch(executable, profile, index)));
    entries = launches.filter((entry) => entry.status === 'fulfilled').map((entry) => entry.value);
    const failed = launches.find((entry) => entry.status === 'rejected');
    if (failed) throw failed.reason;
    const totalLaunchMs = Number(process.hrtime.bigint() - started) / 1e6;
    const peak = countChromeProcesses();
    const closeStarted = process.hrtime.bigint();
    await Promise.all(entries.map(close));
    const closeMs = Number(process.hrtime.bigint() - closeStarted) / 1e6;
    await delay(1000);
    return {
      engine: 'tauri/chromiumoxide',
      level,
      totalLaunchMs: Math.round(totalLaunchMs),
      maxLaunchMs: Math.round(Math.max(...entries.map((entry) => entry.launchMs))),
      meanLaunchMs: Math.round(entries.reduce((sum, entry) => sum + entry.launchMs, 0) / entries.length),
      totalCloseMs: Math.round(closeMs),
      chromeProcessesBefore: before,
      chromeProcessesPeak: peak,
      chromeProcessesAfter: countChromeProcesses(),
    };
  } finally {
    await Promise.all(entries.map(close));
    await fs.rm(root, { recursive: true, force: true }).catch(() => {});
  }
}

async function main() {
  const executable = executablePath();
  await fs.access(executable);
  const requested = process.argv.slice(2).map(Number).filter((value) => Number.isInteger(value) && value > 0 && value <= 16);
  for (const level of requested.length ? requested : [1, 5]) {
    console.log(JSON.stringify(await runLevel(level, executable)));
  }
}

main().catch((error) => {
  console.error(error && error.stack ? error.stack : error);
  process.exitCode = 1;
});
