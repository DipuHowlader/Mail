import React from 'react';
import { useAppContext } from '../context/AppContext.jsx';

const TONE_STYLES = {
  default: 'border-line bg-canvas-elevated',
  success: 'border-emerald-500/30 bg-emerald-950/40',
  error: 'border-red-500/30 bg-red-950/40',
};

const TONE_DOT = {
  default: 'bg-accent-soft',
  success: 'bg-emerald-400',
  error: 'bg-red-400',
};

export default function ToastStack() {
  const { toasts, removeToast } = useAppContext();

  if (!toasts.length) return null;

  return (
    <div className="pointer-events-none fixed bottom-5 right-5 z-[100] flex w-80 flex-col gap-2">
      {toasts.map((toast) => (
        <div
          key={toast.id}
          className={`pointer-events-auto animate-scale-in rounded-lg border px-4 py-3 shadow-panel backdrop-blur ${
            TONE_STYLES[toast.tone] || TONE_STYLES.default
          }`}
        >
          <div className="flex items-start gap-2.5">
            <span className={`mt-1 h-1.5 w-1.5 shrink-0 rounded-full ${TONE_DOT[toast.tone] || TONE_DOT.default}`} />
            <div className="min-w-0 flex-1">
              {toast.title && <p className="text-sm font-medium text-ink">{toast.title}</p>}
              {toast.message && (
                <p className="mt-0.5 select-text break-words text-xs leading-relaxed text-ink-secondary">{toast.message}</p>
              )}
            </div>
            <button
              type="button"
              onClick={() => removeToast(toast.id)}
              className="no-drag shrink-0 rounded p-0.5 text-ink-muted transition hover:bg-white/5 hover:text-ink"
              aria-label="Dismiss"
            >
              <svg width="12" height="12" viewBox="0 0 12 12" fill="none">
                <path d="M1 1l10 10M11 1L1 11" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" />
              </svg>
            </button>
          </div>
        </div>
      ))}
    </div>
  );
}
