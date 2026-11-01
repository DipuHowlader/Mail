'use strict';

/**
 * Run document-conversion benchmarks in the packaged Tauri app. Build the app
 * first with `npm run build:tauri`; no test assertions are performed here.
 * Usage: node scripts/benchmark-document-concurrency-tauri.cjs [1 2 3 4 5]
 */

const path = require('node:path');
const fs = require('node:fs');
const { spawnSync } = require('node:child_process');

const executable = process.env.NOVAMAIL_TAURI_BIN
  || path.resolve('src-tauri', 'target', 'release', process.platform === 'win32' ? 'novamail.exe' : 'novamail');
if (!fs.existsSync(executable)) {
  throw new Error(`Tauri app not found at ${executable}. Run npm run build:tauri first or set NOVAMAIL_TAURI_BIN.`);
}

const levels = process.argv.slice(2).filter((value) => /^\d+$/.test(value)).join(',') || '1,2,3,4,5';
const result = spawnSync(executable, [], {
  encoding: 'utf8',
  env: {
    ...process.env,
    NOVAMAIL_SELFTEST_MODE: 'document-benchmark',
    NOVAMAIL_BENCHMARK_LEVELS: levels,
    NOVAMAIL_WINDOW_HIDDEN: '1',
  },
  windowsHide: true,
});

if (result.stdout) process.stdout.write(result.stdout);
if (result.stderr) process.stderr.write(result.stderr);
if (result.error) throw result.error;
if (result.status !== 0) process.exitCode = result.status || 1;
