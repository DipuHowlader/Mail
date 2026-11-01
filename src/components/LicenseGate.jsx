import React, { useCallback, useEffect, useState } from 'react';
import { licenseApi, isNativeBridgeAvailable } from '../services/ipc.js';
import AppLogo from './AppLogo.jsx';

function licenseErrorMessage(code, detail = '') {
  const normalizedCode = String(code || '').toUpperCase();
  const normalizedDetail = String(detail || '').toLowerCase();
  if (['LICENSE_SERVER_TIMEOUT', 'LICENSE_SERVER_UNREACHABLE', 'LICENSE_SERVER_NOT_CONFIGURED', 'LICENSE_SERVICE_UNAVAILABLE', 'VERIFICATION_UNAVAILABLE'].includes(normalizedCode)
    || /temporarily unavailable|could not reach|network|connection|timeout|fetch failed|und_err|econn/.test(normalizedDetail)) {
    return 'We could not verify your license right now. Check your internet connection and try again.';
  }
  if (normalizedCode.includes('EXPIRED')) return 'This license has expired. Contact support for help renewing it.';
  if (normalizedCode.includes('REVOKED')) return 'This license is no longer active. Contact support for help.';
  if (normalizedCode === 'DEVICE_ID_UNAVAILABLE') return 'We could not read this device\u2019s identifier. Please try again.';
  return 'That license key was not accepted. Check it and try again.';
}

// The license service validates the key as hyphen-separated groups of four
// (`XXXX-XXXX-XXXX-XXXX` or the 32-character `XXXX-XXXX-...` form) over an
// alphabet that omits I, O, 0 and 1. Its `normalizeKey` only trims and
// uppercases — it does NOT strip separators — so the key we send must keep the
// hyphens. The desktop runtime `LicensePage` did exactly this; dropping them made the
// service answer INVALID_INPUT for every key.
function formatKey(value) {
  const cleaned = value
    .toUpperCase()
    .replace(/[^A-Z0-9]/g, '')
    .slice(0, 32);
  return cleaned.match(/.{1,4}/g)?.join('-') ?? cleaned;
}

/**
 * The single license prompt for the whole product.
 *
 * It wraps the launcher (the dashboard). Once a key is accepted here it is
 * stored in the shared config store, so individual apps never ask for it.
 */
export default function LicenseGate({ children }) {
  const [state, setState] = useState('checking'); // checking | locked | unlocked
  const [key, setKey] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');

  const check = useCallback(() => {
    if (!isNativeBridgeAvailable()) {
      // Browser preview: nothing to activate.
      setState('unlocked');
      return;
    }
    licenseApi.checkStored()
      .then((result) => setState(result?.valid ? 'unlocked' : 'locked'))
      .catch(() => setState('locked'));
  }, []);

  useEffect(() => { check(); }, [check]);

  const compactKey = key.replace(/[^A-Z0-9]/g, '');
  const isComplete = compactKey.length === 16 || compactKey.length === 32;

  async function handleActivate(event) {
    event.preventDefault();
    if (!isComplete || busy) return;
    setBusy(true);
    setError('');
    try {
      const result = await licenseApi.verify(key);
      if (!result?.valid) {
        setError(licenseErrorMessage(result?.code, result?.reason));
        return;
      }
      setState('unlocked');
    } catch (err) {
      setError(licenseErrorMessage(err?.code, err?.message));
    } finally {
      setBusy(false);
    }
  }

  if (state === 'unlocked') return children;

  if (state === 'checking') {
    return (
      <div data-theme="dark" className="flex h-full w-full items-center justify-center bg-canvas text-ink">
        <div className="flex items-center gap-3 text-sm text-ink-secondary">
          <span className="h-4 w-4 animate-spin rounded-full border-2 border-line border-t-accent" />
          Checking your license…
        </div>
      </div>
    );
  }

  return (
    <div data-theme="dark" className="relative flex h-full w-full items-center justify-center overflow-hidden bg-canvas text-ink">
      <div className="pointer-events-none absolute inset-0">
        <div className="absolute left-1/2 top-[-10%] h-[560px] w-[560px] -translate-x-1/2 rounded-full bg-accent/10 blur-[140px]" />
      </div>
      <form onSubmit={handleActivate} className="relative z-10 w-full max-w-[420px] rounded-2xl border border-line bg-canvas-elevated/90 p-8 shadow-panel backdrop-blur-xl">
        <div className="flex flex-col items-center text-center">
          <AppLogo className="mb-5 h-11 w-11 rounded-xl bg-gradient-to-br from-accent-soft to-accent shadow-glow" />
          <h1 className="text-lg font-semibold text-ink">Welcome to Novamail</h1>
          <p className="mt-1 text-sm text-ink-secondary">Activate here</p>
          <p className="mt-3 text-xs leading-relaxed text-ink-muted">
            Enter your license key
          </p>
        </div>

        <div className="mt-7">
          <label htmlFor="license-key" className="mb-1.5 block text-[11px] font-medium tracking-wide text-ink-muted">License key</label>
          <input
            id="license-key"
            autoFocus
            value={key}
            onChange={(event) => { setKey(formatKey(event.target.value)); setError(''); }}
            placeholder="XXXX-XXXX-XXXX-XXXX"
            spellCheck={false}
            className="w-full rounded-lg border border-line bg-canvas-secondary px-3 py-3 font-mono text-sm tracking-wider text-ink outline-none transition focus:border-line-accent"
          />
          {error ? <p className="mt-2 text-xs text-red-400">{error}</p> : null}
        </div>

        <button
          type="submit"
          disabled={!isComplete || busy}
          className="mt-5 flex w-full items-center justify-center gap-2 rounded-lg bg-accent px-4 py-3 text-sm font-semibold text-white transition hover:opacity-90 disabled:cursor-not-allowed disabled:bg-line disabled:text-ink-muted"
        >
          {busy ? 'Checking\u2026' : 'Activate'}
        </button>

        <p className="mt-5 border-t border-line pt-4 text-center text-[11px] text-ink-muted">
          Contact the Developer if you need help with your license key.
        </p>
      </form>
    </div>
  );
}
