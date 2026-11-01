import React, { useCallback, useEffect, useMemo, useState } from 'react';
import { launcherApi, isNativeBridgeAvailable } from '../services/ipc.js';
import AppLogo from '../components/AppLogo.jsx';
import { userFacingError } from '../utils/userFacingError.js';

// Plain-English labels for each app state.
const STATE_LABELS = {
  starting: 'Starting up',
  ready: 'Ready',
  running: 'Working',
  sending: 'Sending emails',
  recovering: 'Recovering',
  stopping: 'Closing',
  stopped: 'Closed',
  failed: 'Stopped with a problem',
  orphaned: 'Running (from an earlier session)',
};

const STATE_STYLES = {
  starting: 'bg-amber-400/15 text-amber-200 border-amber-400/30',
  ready: 'bg-emerald-400/15 text-emerald-200 border-emerald-400/30',
  running: 'bg-emerald-400/15 text-emerald-200 border-emerald-400/30',
  sending: 'bg-sky-400/15 text-sky-200 border-sky-400/30',
  recovering: 'bg-amber-400/15 text-amber-200 border-amber-400/30',
  stopping: 'bg-amber-400/15 text-amber-200 border-amber-400/30',
  stopped: 'bg-white/5 text-ink-muted border-line',
  failed: 'bg-red-500/15 text-red-200 border-red-500/30',
  orphaned: 'bg-amber-400/15 text-amber-200 border-amber-400/30',
};

function isActive(state) {
  return ['starting', 'ready', 'running', 'sending', 'recovering', 'stopping', 'orphaned'].includes(state);
}

function StatusPill({ state, lastError }) {
  const label = STATE_LABELS[state] || 'Unknown';
  const style = STATE_STYLES[state] || STATE_STYLES.stopped;
  return (
    <span
      title={lastError || undefined}
      className={`inline-flex items-center gap-1.5 rounded-full border px-2.5 py-0.5 text-[11px] font-medium ${style}`}
    >
      <span className="inline-block h-1.5 w-1.5 rounded-full bg-current opacity-80" />
      {label}
    </span>
  );
}

function AppCard({ item, busy, onOpen, onClose, onRestart, onRemove, onRename, onFocus }) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(item.name);

  useEffect(() => { setDraft(item.name); }, [item.name]);

  const active = isActive(item.state);
  const friendlyError = item.lastError
    ? userFacingError(item.lastError, 'Something went wrong. Please try again.')
    : '';

  const submitRename = async () => {
    setEditing(false);
    const next = draft.trim();
    if (next && next !== item.name) await onRename(item.id, next);
    else setDraft(item.name);
  };

  return (
    <div className="rounded-xl border border-line bg-canvas-elevated p-4">
      <div className="flex items-start justify-between gap-3">
        <div className="flex min-w-0 items-center gap-3">
          <AppLogo className="h-8 w-8 shrink-0" />
          <div className="min-w-0">
            {editing ? (
              <input
                autoFocus
                value={draft}
                onChange={(event) => setDraft(event.target.value)}
                onBlur={submitRename}
                onKeyDown={(event) => {
                  if (event.key === 'Enter') submitRename();
                  if (event.key === 'Escape') { setEditing(false); setDraft(item.name); }
                }}
                className="w-full max-w-[18rem] rounded border border-line-accent bg-canvas px-2 py-0.5 text-sm text-ink outline-none"
              />
            ) : (
              <button
                type="button"
                onClick={() => setEditing(true)}
                title="Click to rename"
                className="block max-w-full truncate text-left text-sm font-semibold text-ink hover:text-accent"
              >
                {item.name}
              </button>
            )}
            <div className="mt-0.5 truncate text-[11px] text-ink-muted">
              {item.accountEmail ? `Gmail: ${item.accountEmail}` : 'No Gmail login yet'}
            </div>
          </div>
        </div>
        <StatusPill state={item.state} lastError={friendlyError} />
      </div>

      {friendlyError && item.state === 'failed' ? (
        <p className="mt-2 rounded border border-red-500/20 bg-red-500/5 px-2 py-1 text-[11px] text-red-200">
          {friendlyError}
        </p>
      ) : null}

      <div className="mt-3 flex flex-wrap items-center gap-2">
        {active && !['stopping'].includes(item.state) ? (
          <>
            <button type="button" disabled={busy} onClick={() => onFocus(item.id)} className="rounded-md border border-line px-3 py-1.5 text-xs font-medium text-ink-secondary transition hover:border-line-accent hover:text-ink disabled:opacity-50">
              Show window
            </button>
            <button type="button" disabled={busy} onClick={() => onRestart(item.id)} className="rounded-md border border-line px-3 py-1.5 text-xs font-medium text-ink-secondary transition hover:border-line-accent hover:text-ink disabled:opacity-50">
              Restart
            </button>
            <button type="button" disabled={busy} onClick={() => onClose(item.id)} className="rounded-md border border-line px-3 py-1.5 text-xs font-medium text-ink-secondary transition hover:border-red-400/50 hover:text-red-200 disabled:opacity-50">
              Close app
            </button>
          </>
        ) : (
          <button type="button" disabled={busy} onClick={() => onOpen(item.id)} className="rounded-md bg-accent px-3 py-1.5 text-xs font-semibold text-white transition hover:opacity-90 disabled:opacity-50">
            Open app
          </button>
        )}
        <button type="button" disabled={busy} onClick={() => onRemove(item.id)} className="ml-auto rounded-md border border-transparent px-3 py-1.5 text-xs font-medium text-ink-muted transition hover:border-red-400/40 hover:text-red-200 disabled:opacity-50">
          Remove
        </button>
      </div>
    </div>
  );
}

export default function LauncherApp() {
  const [apps, setApps] = useState([]);
  const [busyId, setBusyId] = useState('');
  const [error, setError] = useState('');
  const [version, setVersion] = useState('');

  const refresh = useCallback(async () => {
    if (!isNativeBridgeAvailable()) return;
    try {
      setApps(await launcherApi.list());
    } catch (err) {
      setError(userFacingError(err, 'Could not load your apps. Please try again.'));
    }
  }, []);

  useEffect(() => {
    refresh();
    launcherApi.info().then((info) => setVersion(info?.version || '')).catch(() => {});
    if (!isNativeBridgeAvailable()) return undefined;
    return launcherApi.onChanged((next) => setApps(next));
  }, [refresh]);

  const withBusy = (id, fn) => async () => {
    setBusyId(id);
    setError('');
    try {
      await fn();
    } catch (err) {
      setError(userFacingError(err, 'Something went wrong. Please try again.'));
    } finally {
      setBusyId('');
      refresh();
    }
  };

  // Apps are named automatically, so creating one never asks for a name.
  const createApp = withBusy('__new__', async () => {
    await launcherApi.create();
  });

  const runningCount = useMemo(() => apps.filter((item) => isActive(item.state)).length, [apps]);

  const handlers = {
    onOpen: (id) => withBusy(id, () => launcherApi.open(id))(),
    onClose: (id) => withBusy(id, () => launcherApi.close(id))(),
    onRestart: (id) => withBusy(id, () => launcherApi.restart(id))(),
    onRemove: (id) => withBusy(id, () => launcherApi.remove(id))(),
    onFocus: (id) => withBusy(id, () => launcherApi.focus(id))(),
    onRename: (id, name) => withBusy(id, () => launcherApi.rename(id, name))(),
  };

  return (
    <div data-theme="dark" className="flex h-full w-full flex-col bg-canvas text-ink">
      <header className="flex items-center justify-between gap-4 border-b border-line px-6 py-5">
        <div className="flex items-center gap-3">
          <AppLogo className="h-9 w-9" />
          <div>
            <h1 className="text-base font-semibold text-ink">Novamail</h1>
            <p className="text-xs text-ink-muted">
              Each app runs on its own.
            </p>
          </div>
        </div>
        <button
          type="button"
          disabled={busyId === '__new__'}
          onClick={createApp}
          className="rounded-lg bg-accent px-4 py-2 text-sm font-semibold text-white transition hover:opacity-90 disabled:opacity-50"
        >
          {busyId === '__new__' ? 'Creating…' : 'New app'}
        </button>
      </header>

      {error ? (
        <div className="mx-6 mt-4 rounded-lg border border-red-500/30 bg-red-500/10 px-4 py-2 text-xs text-red-200">
          {error}
        </div>
      ) : null}

      <main className="min-h-0 flex-1 overflow-y-auto px-6 py-5">
        <div className="mb-3 flex items-center justify-between text-xs text-ink-muted">
          <span>{apps.length} app{apps.length === 1 ? '' : 's'}</span>
          <span>{runningCount} open</span>
        </div>

        {apps.length === 0 ? (
          <div className="rounded-xl border border-dashed border-line px-6 py-12 text-center">
            <p className="text-sm text-ink-secondary">You don’t have any apps yet.</p>
            <p className="mt-1 text-xs text-ink-muted">
              Create one to get started. You can run several at the same time — each with its own Gmail login.
            </p>
          </div>
        ) : (
          <div className="grid grid-cols-1 gap-3 lg:grid-cols-2">
            {apps.map((item) => (
              <AppCard
                key={item.id}
                item={item}
                busy={busyId === item.id}
                {...handlers}
              />
            ))}
          </div>
        )}
      </main>

      <footer className="border-t border-line px-6 py-3 text-[11px] text-ink-muted">
        Novamail launcher{version ? ` · version ${version}` : ''}
      </footer>
    </div>
  );
}
