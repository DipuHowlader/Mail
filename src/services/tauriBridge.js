/**
 * Novamail — Tauri bridge.
 *
 * The renderer talks to the native layer through exactly one object, `window.api`.
 * In non-Tauri builds this module is skipped; in a
 * Tauri build this module installs the same shape on top of `invoke`/`listen`, so
 * `src/services/ipc.js` and every component stay unchanged.
 *
 * Import order matters: this must run before anything reads `window.api`
 * (`src/services/ipc.js` reads it at module scope). See `src/main.jsx`.
 */
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

const hasTauri = typeof window !== 'undefined'
  && typeof window.__TAURI_INTERNALS__ !== 'undefined';

// Non-Tauri builds install `window.api` themselves, so this is a no-op there.
if (hasTauri && !window.api) {
  const info = window.__NOVAMAIL_RUNTIME__ || {};

  // The native bridge returned a synchronous unsubscribe function from every
  // `on*` method. Tauri's `listen` resolves asynchronously, so hide that behind
  // the same signature.
  const subscribe = (event) => (callback) => {
    let unlisten = null;
    let cancelled = false;
    listen(event, (message) => callback(message.payload))
      .then((fn) => {
        if (cancelled) fn();
        else unlisten = fn;
      })
      .catch(() => {});
    return () => {
      cancelled = true;
      if (unlisten) unlisten();
    };
  };

  window.api = {
    runtime: {
      role: info.role === 'launcher' ? 'launcher' : 'app',
      isLauncher: Boolean(info.isLauncher),
      instanceId: info.instanceId || '',
      instanceName: info.instanceName || '',
    },
    window: {
      minimize: () => invoke('window_minimize'),
      maximize: () => invoke('window_maximize'),
      close: () => invoke('window_close'),
      isMaximized: () => invoke('window_is_maximized'),
      onStateChange: subscribe('window:state'),
    },
    license: {
      verify: (key) => invoke('license_verify', { key }),
      checkStored: () => invoke('license_check_stored'),
    },
    campaigns: {
      save: (campaign) => invoke('campaigns_save', { campaign }),
      list: () => invoke('campaigns_list'),
      delete: (campaignId) => invoke('campaigns_delete', { campaignId }),
    },
    recipients: {
      pickAndParse: () => invoke('recipients_pick_and_parse'),
    },
    templates: {
      pick: () => invoke('templates_pick'),
    },
    subjects: {
      pick: () => invoke('subjects_pick'),
    },
    attachments: {
      pick: () => invoke('attachments_pick'),
    },
    documents: {
      convert: (payload) => invoke('document_convert', { payload }),
    },
    automation: {
      addProfile: (options) => invoke('automation_add_profile', { options }),
      listProfiles: () => invoke('automation_list_profiles'),
      listRuns: () => invoke('automation_list_runs'),
      startAutomation: (payload) => invoke('automation_start', { payload }),
      retryAutomation: (sessionId, instanceId, mode) => invoke('automation_retry', { sessionId, instanceId, mode }),
      stopAutomation: (instanceOrSessionId) => invoke('automation_stop', { instanceOrSessionId }),
      pauseAutomation: (instanceId) => invoke('automation_pause', { instanceId }),
      resumeAutomation: (instanceId) => invoke('automation_resume', { instanceId }),
      closeProfile: (instanceId) => invoke('automation_close_profile', { instanceId }),
      transmissionState: () => invoke('automation_transmission_state'),
      resetAllInstances: () => invoke('automation_reset_all_instances'),
      resetAppData: () => invoke('app_reset_data'),
      onTelemetry: subscribe('automation:telemetry'),
      onProgress: subscribe('mail-merge-progress'),
      onSuccess: subscribe('mail-merge-success'),
      onError: subscribe('mail-merge-error'),
    },
    launcher: {
      list: () => invoke('launcher_list'),
      create: (options) => invoke('launcher_create', { options }),
      open: (id) => invoke('launcher_open', { id }),
      close: (id) => invoke('launcher_close', { id }),
      restart: (id) => invoke('launcher_restart', { id }),
      remove: (id) => invoke('launcher_remove', { id }),
      rename: (id, name) => invoke('launcher_rename', { id, name }),
      focus: (id) => invoke('launcher_focus', { id }),
      logs: (id) => invoke('launcher_logs', { id }),
      info: () => invoke('launcher_info'),
      onChanged: subscribe('launcher:changed'),
      onLog: subscribe('launcher:log'),
    },
  };
}
