function bridge() {
  if (typeof window !== 'undefined' && window.api) return window.api;
  throw new Error('Novamail could not start properly. Please restart the app and try again.');
}

// Which role this window is running in. The launcher window shows the app
// manager; an app window shows one independent app.
export const runtimeInfo = (typeof window !== 'undefined' && window.api?.runtime) || {
  role: 'app',
  isLauncher: false,
  instanceId: '',
  instanceName: '',
};

export const launcherApi = {
  list: () => bridge().launcher.list(),
  create: (options) => bridge().launcher.create(options),
  open: (id) => bridge().launcher.open(id),
  close: (id) => bridge().launcher.close(id),
  restart: (id) => bridge().launcher.restart(id),
  remove: (id) => bridge().launcher.remove(id),
  rename: (id, name) => bridge().launcher.rename(id, name),
  focus: (id) => bridge().launcher.focus(id),
  logs: (id) => bridge().launcher.logs(id),
  info: () => bridge().launcher.info(),
  onChanged: (callback) => bridge().launcher.onChanged(callback),
  onLog: (callback) => bridge().launcher.onLog(callback),
};

export const licenseApi = {
  verify: (key) => bridge().license.verify(key),
  checkStored: () => bridge().license.checkStored(),
};

export const automationApi = {
  addProfile: (options) => bridge().automation.addProfile(options),
  listProfiles: () => bridge().automation.listProfiles(),
  listRuns: () => bridge().automation.listRuns(),
  start: (payload) => bridge().automation.startAutomation(payload),
  retry: (sessionId, instanceId, mode) => bridge().automation.retryAutomation(sessionId, instanceId, mode),
  stop: (instanceOrSessionId) => bridge().automation.stopAutomation(instanceOrSessionId),
  pause: (instanceId) => bridge().automation.pauseAutomation(instanceId),
  resume: (instanceId) => bridge().automation.resumeAutomation(instanceId),
  closeProfile: (instanceId) => bridge().automation.closeProfile(instanceId),
  transmissionState: () => bridge().automation.transmissionState(),
  resetAllInstances: () => bridge().automation.resetAllInstances(),
  resetAppData: () => bridge().automation.resetAppData(),
  onTelemetry: (callback) => bridge().automation.onTelemetry(callback),
  onProgress: (callback) => bridge().automation.onProgress(callback),
  onSuccess: (callback) => bridge().automation.onSuccess(callback),
  onError: (callback) => bridge().automation.onError(callback),
};

export const campaignsApi = {
  save: (campaign) => bridge().campaigns.save(campaign),
  list: () => bridge().campaigns.list(),
  delete: (campaignId) => bridge().campaigns.delete(campaignId),
};

export const recipientsApi = {
  pickAndParse: () => bridge().recipients.pickAndParse(),
};

export const templatesApi = {
  pick: () => bridge().templates.pick(),
};

export const subjectsApi = {
  pick: () => bridge().subjects.pick(),
};

export const attachmentsApi = {
  pick: () => bridge().attachments.pick(),
};

export const documentsApi = {
  convert: (payload) => bridge().documents.convert(payload),
};

export const windowApi = {
  minimize: () => bridge().window.minimize(),
  maximize: () => bridge().window.maximize(),
  close: () => bridge().window.close(),
  isMaximized: () => bridge().window.isMaximized(),
  onStateChange: (cb) => bridge().window.onStateChange(cb),
};

export function isNativeBridgeAvailable() {
  return typeof window !== 'undefined' && Boolean(window.api);
}
