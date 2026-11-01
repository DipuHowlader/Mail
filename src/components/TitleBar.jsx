import React, { useEffect, useState } from 'react';
import { windowApi, isNativeBridgeAvailable } from '../services/ipc.js';
import AppLogo from './AppLogo.jsx';

export default function TitleBar({ label = 'Novamail' }) {
  const [maximized, setMaximized] = useState(false);

  useEffect(() => {
    if (!isNativeBridgeAvailable()) return undefined;
    windowApi.isMaximized().then(setMaximized).catch(() => {});
    const unsubscribe = windowApi.onStateChange((state) => setMaximized(state.maximized));
    return unsubscribe;
  }, []);

  const handle = (action) => () => {
    if (!isNativeBridgeAvailable()) return;
    action().catch(() => {});
  };

  return (
    <div className="drag flex h-9 shrink-0 items-center justify-between border-b border-line bg-canvas-secondary pl-3 pr-0">
      <div className="flex items-center gap-2">
        <AppLogo className="h-5 w-5" />
        <span className="text-xs font-medium text-ink-secondary">{label}</span>
      </div>

      <div className="no-drag flex h-full items-stretch">
        <button
          type="button"
          aria-label="Minimize"
          onClick={handle(windowApi.minimize)}
          className="flex w-11 items-center justify-center text-ink-muted transition hover:bg-white/5 hover:text-ink"
        >
          <svg width="10" height="10" viewBox="0 0 10 10"><path d="M1 5h8" stroke="currentColor" strokeWidth="1" /></svg>
        </button>
        <button
          type="button"
          aria-label="Maximize"
          onClick={handle(windowApi.maximize)}
          className="flex w-11 items-center justify-center text-ink-muted transition hover:bg-white/5 hover:text-ink"
        >
          {maximized ? (
            <svg width="10" height="10" viewBox="0 0 10 10">
              <path d="M2.5 0.5h6v6h-6z" stroke="currentColor" strokeWidth="1" fill="none" transform="translate(1,-1)" />
              <path d="M0.5 2.5h6v6h-6z" stroke="currentColor" strokeWidth="1" fill="none" />
            </svg>
          ) : (
            <svg width="10" height="10" viewBox="0 0 10 10">
              <rect x="0.5" y="0.5" width="9" height="9" stroke="currentColor" strokeWidth="1" fill="none" />
            </svg>
          )}
        </button>
        <button
          type="button"
          aria-label="Close"
          onClick={handle(windowApi.close)}
          className="flex w-11 items-center justify-center text-ink-muted transition hover:bg-red-600 hover:text-white"
        >
          <svg width="10" height="10" viewBox="0 0 10 10"><path d="M0.5 0.5l9 9M9.5 0.5l-9 9" stroke="currentColor" strokeWidth="1" /></svg>
        </button>
      </div>
    </div>
  );
}
