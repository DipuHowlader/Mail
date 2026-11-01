import React, { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState } from 'react';
import { automationApi, isNativeBridgeAvailable } from '../services/ipc.js';
import { userFacingError } from '../utils/userFacingError.js';

const AppContext = createContext(null);

let toastSeq = 0;

function mergeAutomationTelemetry(previous, event) {
  const newSession = Boolean(previous?.sessionId && event?.sessionId && previous.sessionId !== event.sessionId);
  const timestamp = Number.isFinite(event?.timestamp) ? event.timestamp : Date.now();
  const entry = {
    ...event,
    timestamp,
    event: event?.event || event?.status || 'unknown',
    recipient: event?.recipient || event?.activeRecipient || null,
    activeRecipient: event?.recipient || event?.activeRecipient || null,
    logs: typeof event?.logs === 'string' ? event.logs : Array.isArray(event?.logs) ? event.logs.join(' ') : '',
  };
  const history = !newSession && Array.isArray(previous?.logs) ? previous.logs : [];
  const latestByKey = new Map();
  for (const item of [...history, entry]) {
    const recipient = typeof item?.recipient === 'string' ? item.recipient.trim().toLowerCase() : '';
    const key = recipient
      ? `recipient:${item.recipientId || recipient}`
      : `status:${item?.event || item?.status || 'unknown'}`;
    const existing = latestByKey.get(key);
    if (!existing || (item.timestamp || 0) >= (existing.timestamp || 0)) latestByKey.set(key, item);
  }
  const logs = [...latestByKey.values()]
    .sort((left, right) => (left.timestamp || 0) - (right.timestamp || 0))
    .slice(-100);
  const isNewer = newSession || !previous?.timestamp || timestamp >= previous.timestamp;
  return {
    ...previous,
    ...(isNewer ? entry : {}),
    logs,
  };
}

export function AppProvider({ children }) {
  const [stage, setStage] = useState('booting');
  const [accounts, setAccounts] = useState([]);
  const [activeAccountId, setActiveAccountId] = useState(null);
  const [toasts, setToasts] = useState([]);
  const [composerDraft, setComposerDraft] = useState(null);
  const [theme, setTheme] = useState(() => {
    try { return localStorage.getItem('novamail-theme') === 'light' ? 'light' : 'dark'; } catch { return 'dark'; }
  });
  const [telemetryByInstance, setTelemetryByInstance] = useState({});
  const deletedTelemetryInstances = useRef(new Set());
  const telemetrySnapshotEpoch = useRef(0);

  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    try { localStorage.setItem('novamail-theme', theme); } catch { /* Storage can be unavailable in browser previews. */ }
  }, [theme]);

  const removeTelemetryForInstance = useCallback((instanceId) => {
    if (!instanceId) return;
    deletedTelemetryInstances.current.add(instanceId);
    setTelemetryByInstance((current) => {
      if (!current[instanceId]) return current;
      const next = { ...current };
      delete next[instanceId];
      return next;
    });
  }, []);

  const clearTelemetry = useCallback(() => {
    telemetrySnapshotEpoch.current += 1;
    setTelemetryByInstance((current) => {
      Object.keys(current).forEach((instanceId) => deletedTelemetryInstances.current.add(instanceId));
      return {};
    });
  }, []);

  // An app never gates on the license. The launcher (dashboard) is the only
  // place the key is entered, so an app process goes straight to the composer.
  useEffect(() => {
    setStage('composer');
  }, []);

  useEffect(() => {
    if (!isNativeBridgeAvailable()) return undefined;
    let mounted = true;
    const removeTelemetryListener = automationApi.onTelemetry((event) => {
      const instanceId = typeof event?.instanceId === 'string' ? event.instanceId : event?.accountId;
      if (!instanceId) return;
      if (event.status === 'browser-closed') {
        // Closing Chromium also drops its cached progress and prevents a late
        // worker event from recreating browser-specific state.
        removeTelemetryForInstance(instanceId);
        setAccounts((current) => {
          const next = current.filter((item) => item.id !== instanceId);
          if (next.length !== current.length) {
            setActiveAccountId((active) => (active === instanceId ? next[0]?.id ?? null : active));
          }
          return next;
        });
        return;
      }
      if (deletedTelemetryInstances.current.has(instanceId)) return;
      setTelemetryByInstance((current) => ({
        ...current,
        [instanceId]: mergeAutomationTelemetry(current[instanceId] || {}, {
          ...event,
          instanceId,
          sessionId: event.sessionId || current[instanceId]?.sessionId || '',
          campaignId: event.campaignId || current[instanceId]?.campaignId || '',
          slotId: event.slotId || current[instanceId]?.slotId || '',
          workerId: event.workerId || current[instanceId]?.workerId || '',
        }),
      }));
      if (event.status === 'ready' || event.status === 'waiting-for-login') {
        setAccounts((current) => {
          const account = {
            id: instanceId,
            email: event.accountEmail || '',
            name: event.accountEmail || 'Gmail window',
            ready: event.status === 'ready',
            processId: event.processId || null,
            status: event.status,
            pending: event.status === 'waiting-for-login',
          };
          return current.some((item) => item.id === instanceId)
            ? current.map((item) => item.id === instanceId ? { ...item, ...account } : item)
            : [...current, account];
        });
      } else if (event.status === 'disconnected') {
        // A closed or disconnected Chromium window is removed immediately so the UI
        // never keeps a stale "always present" browser window around.
        setAccounts((current) => {
          const next = current.filter((item) => item.id !== instanceId);
          if (next.length !== current.length) {
            setActiveAccountId((active) => (active === instanceId ? next[0]?.id ?? null : active));
          }
          return next;
        });
      }
    });

    const snapshotEpoch = telemetrySnapshotEpoch.current;
    automationApi.listRuns().then((runs) => {
      if (!mounted || snapshotEpoch !== telemetrySnapshotEpoch.current || !Array.isArray(runs)) return;
      setTelemetryByInstance((current) => {
        const next = { ...current };
        for (const run of runs) {
          if (!run?.instanceId) continue;
          if (deletedTelemetryInstances.current.has(run.instanceId)) continue;
          if (!next[run.instanceId]) next[run.instanceId] = mergeAutomationTelemetry({}, run);
        }
        return next;
      });
    }).catch(() => {});

    return () => {
      mounted = false;
      removeTelemetryListener?.();
    };
  }, [removeTelemetryForInstance]);

  const removeToast = useCallback((id) => {
    setToasts((prev) => prev.filter((t) => t.id !== id));
  }, []);

  const pushToast = useCallback(
    (toast) => {
      const id = ++toastSeq;
      const { error, ...fields } = toast || {};
      const record = {
        id,
        tone: 'default',
        duration: 4000,
        ...fields,
      };
      // The message shown to the user is always the friendly one the caller built
      // (see userFacingError). Raw error objects are accepted for logging only and
      // never rendered, so technical details stay out of the UI.
      if (error) console.error('[novamail]', error);
      setToasts((prev) => [...prev, record]);
      if (record.duration) {
        setTimeout(() => removeToast(id), record.duration);
      }
      return id;
    },
    [removeToast]
  );

  const activeAccount = useMemo(
    () => accounts.find((a) => a.id === activeAccountId) || null,
    [accounts, activeAccountId]
  );

  const upsertAccounts = useCallback(
    (list) => {
      setAccounts(list);
      setActiveAccountId((current) => {
        if (current && list.some((a) => a.id === current)) return current;
        return list[0]?.id ?? null;
      });
    },
    [setAccounts]
  );

  const upsertAccount = useCallback((account) => {
    if (!account?.id) return;
    setAccounts((current) => current.some((item) => item.id === account.id)
      ? current.map((item) => item.id === account.id ? { ...item, ...account } : item)
      : [...current, account]);
  }, []);

  const closeAccountTab = useCallback(
    async (accountId) => {
      try {
        if (isNativeBridgeAvailable()) await automationApi.closeProfile(accountId);
      } catch (error) {
        pushToast({ tone: 'error', title: 'Could not close Gmail account', message: userFacingError(error, 'Please try again.'), error });
        return;
      }
      removeTelemetryForInstance(accountId);
      setAccounts((prev) => {
        const next = prev.filter((a) => a.id !== accountId);
        setActiveAccountId((current) => (current === accountId ? next[0]?.id ?? null : current));
        return next;
      });
    },
    [pushToast, removeTelemetryForInstance]
  );

  const openComposer = useCallback((draft = null) => {
    setComposerDraft(draft);
    setStage('composer');
  }, []);

  const updateComposerDraftFields = useCallback((campaignId, fields) => {
    setComposerDraft((current) => current?.id === campaignId
      ? { ...current, ...fields, id: campaignId }
      : current);
  }, []);

  const clearComposerDraft = useCallback(() => {
    setComposerDraft(null);
  }, []);

  const value = useMemo(
    () => ({
      stage,
      setStage,
      accounts,
      setAccounts: upsertAccounts,
      upsertAccount,
      closeAccountTab,
      activeAccountId,
      setActiveAccountId,
      activeAccount,
      toasts,
      pushToast,
      removeToast,
      composerDraft,
      theme,
      setTheme,
      updateComposerDraftFields,
      clearComposerDraft,
      telemetryByInstance,
      setTelemetryByInstance,
      removeTelemetryForInstance,
      clearTelemetry,
      openComposer,
    }),
    [
      stage,
      accounts,
      upsertAccounts,
      upsertAccount,
      closeAccountTab,
      activeAccountId,
      activeAccount,
      toasts,
      pushToast,
      removeToast,
      composerDraft,
      theme,
      updateComposerDraftFields,
      clearComposerDraft,
      telemetryByInstance,
      setTelemetryByInstance,
      removeTelemetryForInstance,
      clearTelemetry,
      openComposer,
    ]
  );

  return <AppContext.Provider value={value}>{children}</AppContext.Provider>;
}

export function useAppContext() {
  const ctx = useContext(AppContext);
  if (!ctx) throw new Error('useAppContext must be used within AppProvider');
  return ctx;
}
