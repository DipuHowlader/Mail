import React, { useState } from 'react';
import { recipientsApi } from '../services/ipc.js';
import { userFacingError } from '../utils/userFacingError.js';

export default function RecipientImport({ open, onClose, onConfirm }) {
  const [status, setStatus] = useState('idle'); // idle | parsing | reviewing | error
  const [result, setResult] = useState(null);
  const [error, setError] = useState('');
  const [excluded, setExcluded] = useState(() => new Set());

  if (!open) return null;

  async function handlePick() {
    setStatus('parsing');
    setError('');
    try {
      const parsed = await recipientsApi.pickAndParse();
      if (!parsed) {
        setStatus('idle');
        return;
      }
      setResult(parsed);
      setExcluded(new Set());
      setStatus('reviewing');
    } catch (err) {
      setStatus('error');
      setError(userFacingError(err, 'Could not read this file. Choose a valid CSV or Excel file and try again.'));
    }
  }

  function toggleExcluded(email) {
    setExcluded((prev) => {
      const next = new Set(prev);
      if (next.has(email)) next.delete(email);
      else next.add(email);
      return next;
    });
  }

  function handleConfirm() {
    const kept = result.recipients.filter((r) => !excluded.has(r.email));
    onConfirm(kept);
    setStatus('idle');
    setResult(null);
  }

  const keptCount = result ? result.recipients.length - excluded.size : 0;

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm">
      <div className="w-full max-w-md animate-scale-in rounded-xl border border-line bg-canvas-elevated shadow-panel">
        <div className="flex items-center justify-between border-b border-line px-5 py-4">
          <h3 className="text-sm font-semibold text-ink">Import recipients</h3>
          <button type="button" onClick={onClose} className="rounded p-1 text-ink-muted transition hover:bg-white/5 hover:text-ink">
            <svg width="12" height="12" viewBox="0 0 12 12"><path d="M1 1l10 10M11 1L1 11" stroke="currentColor" strokeWidth="1.3" /></svg>
          </button>
        </div>

        <div className="px-5 py-5">
          {status !== 'reviewing' && (
            <>
              <p className="text-xs leading-relaxed text-ink-secondary">
                Upload a CSV or XLSX file. Novamail looks for an email column automatically and adds the selected addresses to To.
              </p>

              <button
                type="button"
                onClick={handlePick}
                disabled={status === 'parsing'}
                className="mt-4 flex w-full flex-col items-center gap-2 rounded-lg border border-dashed border-line py-8 text-center transition hover:border-line-accent hover:bg-white/[0.02] disabled:opacity-60"
              >
                {status === 'parsing' ? (
                  <span className="h-5 w-5 animate-spin rounded-full border-2 border-line border-t-accent-soft" />
                ) : (
                  <svg width="22" height="22" viewBox="0 0 22 22" fill="none">
                    <path d="M11 3v11M6 9.5L11 4.5l5 5" stroke="#94a3b8" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" />
                    <path d="M4 15v2.5a1.5 1.5 0 001.5 1.5h11a1.5 1.5 0 001.5-1.5V15" stroke="#94a3b8" strokeWidth="1.4" strokeLinecap="round" />
                  </svg>
                )}
                <span className="text-xs font-medium text-ink-secondary">
                  {status === 'parsing' ? 'Reading file…' : 'CSV or XLSX'}
                </span>
              </button>

              {status === 'error' && <p className="mt-3 text-xs text-red-400">{error}</p>}
            </>
          )}

          {status === 'reviewing' && result && (
            <>
              <div className="flex items-center gap-2 rounded-lg border border-line bg-canvas-secondary px-3 py-2.5">
                <span className="text-emerald-400">
                  <svg width="14" height="14" viewBox="0 0 14 14"><path d="M2.5 7.5l3 3 6-6.5" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" fill="none" /></svg>
                </span>
                <div className="min-w-0 flex-1">
                  <p className="truncate text-xs font-medium text-ink">{result.fileName}</p>
                  <p className="text-[11px] text-ink-muted">{result.recipients.length} recipients found</p>
                </div>
              </div>

              <p className="mt-3 text-[11px] text-ink-muted">Review before adding — uncheck anyone you don't want to include.</p>

              <div className="mt-2 max-h-56 overflow-y-auto rounded-lg border border-line">
                {result.recipients.map((r) => (
                  <label
                    key={r.email}
                    className="flex cursor-pointer items-center gap-2.5 border-b border-line px-3 py-2 last:border-b-0 hover:bg-white/[0.02]"
                  >
                    <input
                      type="checkbox"
                      checked={!excluded.has(r.email)}
                      onChange={() => toggleExcluded(r.email)}
                      className="h-3.5 w-3.5 rounded border-line accent-accent"
                    />
                    <span className="min-w-0 flex-1 truncate text-xs text-ink">{r.email}</span>
                    {r.name && <span className="shrink-0 truncate text-[11px] text-ink-muted">{r.name}</span>}
                  </label>
                ))}
              </div>
            </>
          )}
        </div>

        {status === 'reviewing' && (
          <div className="flex items-center justify-between border-t border-line px-5 py-4">
            <button type="button" onClick={handlePick} className="text-xs font-medium text-ink-secondary transition hover:text-ink">
              Choose a different file
            </button>
            <button
              type="button"
              onClick={handleConfirm}
              disabled={keptCount === 0}
              className="rounded-lg bg-accent px-4 py-2 text-xs font-semibold text-white transition hover:bg-accent-soft disabled:cursor-not-allowed disabled:bg-line disabled:text-ink-muted"
            >
              Add {keptCount} recipient{keptCount === 1 ? '' : 's'}
            </button>
          </div>
        )}
      </div>
    </div>
  );
}
