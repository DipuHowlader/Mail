import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useAppContext } from '../context/AppContext.jsx';
import AttachmentList from '../components/AttachmentList.jsx';
import RecipientImport from '../components/RecipientImport.jsx';
import { automationApi, campaignsApi, attachmentsApi, documentsApi, isNativeBridgeAvailable, runtimeInfo } from '../services/ipc.js';
import { isValidEmail, formatTime } from '../utils/validation.js';
import { parseTemplate } from '../utils/templateEngine.js';
import { userFacingError } from '../utils/userFacingError.js';
import TagsPage from './TagsPage.jsx';

// True when this window is a single, independent app launched from the
// launcher (it has its own instance id). In that case the composer is pinned to
// exactly one browser window, because "one app" means one Gmail login, one
// browser and one telemetry pipeline. Standalone/dev runs keep the multi-window
// composer.
const APP_MODE = Boolean(runtimeInfo.instanceId) && !runtimeInfo.isLauncher;

function isHtmlTemplateFile(file) {
  return [file?.filename, file?.path].some((value) => /\.html?$/i.test(String(value || '')));
}

function dedupeAdd(list, email, metadata = {}) {
  const trimmed = email.trim();
  if (!trimmed) return list;
  if (list.some((r) => r.email.toLowerCase() === trimmed.toLowerCase())) return list;
  return [...list, { ...metadata, email: trimmed }];
}

function decodeBase64Bytes(value) {
  const binary = atob(String(value || ''));
  return Uint8Array.from(binary, (character) => character.charCodeAt(0));
}

function formatFileSize(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(2)} MB`;
}

const DEFAULT_FILE_NAME_TEMPLATE = 'Invoice_$invoice_no_$username_$customer_id';
const FILE_NAME_PATTERN_PRESETS = [
  { label: 'Invoice with recipient and customer', pattern: 'Invoice_$invoice_no_$username_$customer_id' },
  { label: 'Invoice and customer', pattern: 'Invoice_$invoice_no_$customer_id' },
  { label: 'Company invoice', pattern: '$company_name_Invoice_$invoice_no' },
  { label: 'Order and recipient', pattern: 'Order_$order_id_$username' },
  { label: 'Receipt and date', pattern: 'Receipt_$transaction_id_$date' },
  { label: 'Recipient and date', pattern: '$username_$date' },
];
const FILE_NAME_TAGS = [
  ['$invoice_no', 'Invoice number'],
  ['$username', 'Recipient name'],
  ['$customer_id', 'Customer ID'],
  ['$order_id', 'Order ID'],
  ['$transaction_id', 'Transaction ID'],
  ['$date', 'Current date'],
  ['$email', 'Recipient email'],
  ['$name', 'Imported recipient name'],
  ['$company_name', 'Company name'],
  ['$number6', 'Six digit number'],
];

function decodeBase64Utf8(value) {
  return new TextDecoder('utf-8').decode(decodeBase64Bytes(value));
}

function normalizeRecipientRecords(records) {
  const recipients = new Map();
  for (const record of records || []) {
    const candidate = typeof record === 'string' ? { email: record } : record;
    const email = String(candidate?.email || '').trim();
    if (!isValidEmail(email) || recipients.has(email.toLowerCase())) continue;
    recipients.set(email.toLowerCase(), { ...candidate, email });
  }
  return [...recipients.values()];
}

function recipientsFromText(value) {
  return normalizeRecipientRecords(String(value || '').split(/[\s,;]+/).map((email) => ({ email })));
}

function draftRecipients(draft) {
  const slotRecipients = (draft?.instanceSettings || []).flatMap((slot) => slot?.recipients || []);
  if (slotRecipients.length) return normalizeRecipientRecords(slotRecipients);
  const metadata = new Map((draft?.recipientData || []).map((recipient) => [String(recipient?.email || '').toLowerCase(), recipient]));
  return normalizeRecipientRecords((draft?.to || []).map((email) => metadata.get(String(email).toLowerCase()) || { email }));
}

function initialInstanceSettings(draft, activeAccountId, singleWindow = false) {
  if (Array.isArray(draft?.instanceSettings) && draft.instanceSettings.length) {
    const legacyRecipients = draftRecipients(draft);
    const mapped = draft.instanceSettings.map((slot, index) => ({
      ...slot,
      accountId: slot.accountId || (index === 0 ? activeAccountId : null),
      recipients: normalizeRecipientRecords(slot.recipients?.length ? slot.recipients : (index === 0 ? legacyRecipients : [])),
    }));
    return singleWindow ? [mapped[0]] : mapped;
  }
  // Existing drafts retain their recipients in one instance; new drafts begin
  // empty and require Add instance before any receiver data is entered.
  const legacyRecipients = draftRecipients(draft);
  if (singleWindow) {
    // An app always has exactly one browser window, created up front so the
    // user can assign its Gmail login and recipients straight away.
    return [{ id: crypto.randomUUID(), accountId: activeAccountId || null, recipients: legacyRecipients }];
  }
  return legacyRecipients.length ? [{ id: crypto.randomUUID(), accountId: activeAccountId || null, recipients: legacyRecipients }] : [];
}

function telemetryStatusLabel(status) {
  return {
    queued: 'Waiting to start',
    waiting: 'Waiting',
    'waiting-for-login': 'Waiting for Gmail sign-in',
    generating: 'Making file',
    uploading: 'Attaching file',
    sending: 'Sending',
    sent: 'Sent',
    failed: 'Needs attention',
    paused: 'Paused',
    pausing: 'Pausing',
    stopping: 'Stopping',
    disconnected: 'Gmail not connected',
    'browser-closed': 'Gmail window closed',
    retrying: 'Trying again',
    retried: 'Trying again',
    aborted: 'Stopped',
    error: 'Needs attention',
    completed: 'Done',
  }[status] || 'Waiting';
}

const TELEMETRY_WORKER_BUSY_STATUSES = new Set([
  'queued', 'running', 'waiting-for-login', 'waiting', 'generating', 'uploading',
  'sending', 'paused', 'pausing', 'stopping', 'retrying',
]);

// Pull the failure reason out of a telemetry entry and put it in plain language.
// The backend sends a dedicated `error` field, but entries recorded before that was
// added (or replayed from history) only carry the reason inside the log line, so fall
// back to parsing it out. Raw backend text is never shown as-is.
function telemetryEntryError(entry) {
  if (!entry) return '';
  let raw = '';
  if (typeof entry.error === 'string' && entry.error.trim()) raw = entry.error.trim();
  else {
    const logs = typeof entry.logs === 'string' ? entry.logs : '';
    const match = logs.match(/^(?:Failed for [^:]+|Instance worker error):\s*(.+)$/i);
    raw = match ? match[1].trim() : '';
  }
  if (!raw) return '';
  return userFacingError(raw, 'Something went wrong. Please try again.');
}

export default function ComposerPage() {
  const {
    accounts, setAccounts, activeAccountId, composerDraft,
    theme, setTheme, pushToast, telemetryByInstance, removeTelemetryForInstance, clearTelemetry, updateComposerDraftFields, clearComposerDraft,
  } = useAppContext();

  const [to, setTo] = useState(() => {
    // An app owns exactly one window, so its recipient list comes from that one
    // window rather than from every window saved in the draft.
    if (APP_MODE) {
      const [slot] = initialInstanceSettings(composerDraft, activeAccountId, true);
      return normalizeRecipientRecords(slot?.recipients || []);
    }
    return draftRecipients(composerDraft);
  });
  const [cc, setCc] = useState(composerDraft?.cc?.map((email) => ({ email })) || []);
  const [bcc, setBcc] = useState(composerDraft?.bcc?.map((email) => ({ email })) || []);
  const [showCcBcc, setShowCcBcc] = useState(Boolean(composerDraft?.cc?.length || composerDraft?.bcc?.length));
  const [bodyHtml, setBodyHtml] = useState(composerDraft?.bodyHtml || '');
  const [rawHtmlTemplate, setRawHtmlTemplate] = useState(composerDraft?.rawHtmlTemplate || '');
  const [templateSearchOpen, setTemplateSearchOpen] = useState(false);
  const [templateSearch, setTemplateSearch] = useState('');
  const [templateSearchMatchIndex, setTemplateSearchMatchIndex] = useState(-1);
  const [templateSearchMessage, setTemplateSearchMessage] = useState('');
  const [tagsOpen, setTagsOpen] = useState(false);
  const [fileNameTagsOpen, setFileNameTagsOpen] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [instanceRecipientDialogOpen, setInstanceRecipientDialogOpen] = useState(false);
  const [instanceRecipientText, setInstanceRecipientText] = useState('');
  const [instanceImportedRecipients, setInstanceImportedRecipients] = useState([]);
  const [instanceImportOpen, setInstanceImportOpen] = useState(false);
  const htmlTemplateRef = useRef(null);
  const htmlTemplateHighlightRef = useRef(null);
  const [attachments, setAttachments] = useState(() => (composerDraft?.attachments || []).filter((file) => !isHtmlTemplateFile(file)));
  const [mode, setMode] = useState('edit'); // edit | preview
  const [draftId, setDraftId] = useState(composerDraft?.id || null);
  const draftIdRef = useRef(draftId);
  const draftSaveEpochRef = useRef(0);
  const resetAttemptRef = useRef(null);
  draftIdRef.current = draftId;
  const [draftSavedAt, setDraftSavedAt] = useState(composerDraft?.savedAt || null);
  const [savingDraft, setSavingDraft] = useState(false);
  const [draftSaveError, setDraftSaveError] = useState(false);
  const [baseAmount, setBaseAmount] = useState(composerDraft?.baseAmount || '');
  const [tfn, setTfn] = useState(composerDraft?.tfn || '');
  const [companyName, setCompanyName] = useState(composerDraft?.companyName || '');
  const [subjectTemplate, setSubjectTemplate] = useState(composerDraft?.subjectTemplate || composerDraft?.subject || '');
  const [outputFormat, setOutputFormat] = useState(() => {
    const savedFormat = String(composerDraft?.outputFormat || '').trim().toLowerCase();
    return ['inline', 'inline-html', 'inline-text', 'html'].includes(savedFormat)
      ? 'inline-html'
      : savedFormat || 'inline-html';
  });
  const [autoGenerateFile, setAutoGenerateFile] = useState(composerDraft?.autoGenerateFile ?? false);
  const [fileNameTemplate, setFileNameTemplate] = useState(composerDraft?.fileNameTemplate || DEFAULT_FILE_NAME_TEMPLATE);
  const fileNameInputRef = useRef(null);
  const fileNameSelectionRef = useRef(null);
  const [preview, setPreview] = useState(null);
  const [previewLoading, setPreviewLoading] = useState(false);
  const bodyText = useMemo(() => bodyHtml.replace(/<style[\s\S]*?<\/style>|<script[\s\S]*?<\/script>|<[^>]+>/gi, ' ').replace(/\s+/g, ' ').trim(), [bodyHtml]);
  const dueDatePreview = new Date();
  dueDatePreview.setDate(dueDatePreview.getDate() + 30);
  const dueDateLabel = new Intl.DateTimeFormat('en-US', { month: 'short', day: 'numeric', year: 'numeric' }).format(dueDatePreview);
  const subject = useMemo(() => parseTemplate(subjectTemplate, {
    baseAmount,
    tfn,
    companyName,
    company_name: companyName,
    due_date: dueDateLabel,
    email: to[0]?.email || 'recipient@example.com',
    ...(to[0] || {}),
    sender_email: accounts.find((account) => account.id === activeAccountId)?.email || '',
  }), [subjectTemplate, baseAmount, tfn, companyName, dueDateLabel, to, accounts, activeAccountId]);
  const previewSource = rawHtmlTemplate.trim() ? rawHtmlTemplate : bodyHtml;
  const previewHtml = useMemo(() => parseTemplate(previewSource, {
    baseAmount,
    tfn,
    companyName,
    company_name: companyName,
    due_date: dueDateLabel,
    email: to[0]?.email || 'recipient@example.com',
    ...(to[0] || {}),
    sender_email: accounts.find((account) => account.id === activeAccountId)?.email || '',
  }), [previewSource, baseAmount, tfn, companyName, dueDateLabel, to, accounts, activeAccountId]);
  const templateSearchMatches = useMemo(() => {
    const query = templateSearch.trim();
    if (!templateSearchOpen || !query) return [];

    const escapedQuery = query.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
    const expression = new RegExp(escapedQuery, 'giu');
    const matches = [];
    let match;
    while ((match = expression.exec(rawHtmlTemplate))) {
      matches.push({ start: match.index, end: match.index + match[0].length });
    }
    return matches;
  }, [rawHtmlTemplate, templateSearch, templateSearchOpen]);
  const templateSearchLayer = useMemo(() => {
    if (!templateSearchMatches.length) return `${rawHtmlTemplate}\u200b`;

    const pieces = [];
    let cursor = 0;
    templateSearchMatches.forEach(({ start, end }, index) => {
      pieces.push(<React.Fragment key={`before-${index}`}>{rawHtmlTemplate.slice(cursor, start)}</React.Fragment>);
      pieces.push(
        <mark
          key={`match-${index}`}
          data-search-match-index={index}
          className={index === templateSearchMatchIndex
            ? 'rounded-sm bg-yellow-300 text-slate-950 ring-1 ring-yellow-200/80'
            : 'rounded-sm bg-amber-300/40 text-accent-soft'}
        >
          {rawHtmlTemplate.slice(start, end)}
        </mark>
      );
      cursor = end;
    });
    pieces.push(<React.Fragment key="after-matches">{rawHtmlTemplate.slice(cursor)}{'\u200b'}</React.Fragment>);
    return pieces;
  }, [rawHtmlTemplate, templateSearchMatches, templateSearchMatchIndex]);
  const syncTemplateHighlightScroll = useCallback((textarea = htmlTemplateRef.current) => {
    const highlightLayer = htmlTemplateHighlightRef.current;
    if (!textarea || !highlightLayer) return;
    const scrollbarWidth = Math.max(0, textarea.offsetWidth - textarea.clientWidth - 2);
    highlightLayer.style.paddingRight = `${12 + scrollbarWidth}px`;
    highlightLayer.style.transform = `translate(${-textarea.scrollLeft}px, ${-textarea.scrollTop}px)`;
  }, []);
  const [telemetryWindows, setTelemetryWindows] = useState({});
  const [nextTelemetryZ, setNextTelemetryZ] = useState(100);
  const telemetryDragRef = useRef(null);
  const retryingTelemetryRef = useRef(new Set());
  const [retryingTelemetryInstances, setRetryingTelemetryInstances] = useState(new Set());
  const [activeTelemetryInstanceId, setActiveTelemetryInstanceId] = useState(null);
  const [launchingSlotIds, setLaunchingSlotIds] = useState([]);
  const launchingSlotIdsRef = useRef(new Set());
  const [launchErrors, setLaunchErrors] = useState({});
  const autoOpenedSessions = useRef(new Set());
  const [instanceSettings, setInstanceSettings] = useState(() => initialInstanceSettings(composerDraft, activeAccountId, APP_MODE));
  const [resetDialogOpen, setResetDialogOpen] = useState(false);
  const [resettingAppData, setResettingAppData] = useState(false);
  const [transmissionActive, setTransmissionActive] = useState(false);

  useEffect(() => {
    if (!isNativeBridgeAvailable()) return undefined;
    let mounted = true;
    const refresh = async () => {
      try {
        const state = await automationApi.transmissionState();
        if (mounted) setTransmissionActive(Boolean(state?.transmitting));
      } catch {
        // Keep the last known backend state if polling fails.
      }
    };
    refresh();
    const interval = window.setInterval(refresh, 500);
    return () => {
      mounted = false;
      window.clearInterval(interval);
    };
  }, []);
  const instanceSettingsRef = useRef(instanceSettings);
  instanceSettingsRef.current = instanceSettings;
  const knownAccountIdsRef = useRef(new Set(accounts.map((account) => account.id)));
  useEffect(() => {
    const currentAccountIds = new Set(accounts.map((account) => account.id));
    const closedAccountIds = [...knownAccountIdsRef.current].filter((id) => !currentAccountIds.has(id));
    knownAccountIdsRef.current = currentAccountIds;
    if (closedAccountIds.length === 0) return;

    const closedIds = new Set(closedAccountIds);
    const nextSettings = instanceSettingsRef.current.map((slot) => (
      closedIds.has(slot.accountId) ? { ...slot, accountId: null } : slot
    ));
    instanceSettingsRef.current = nextSettings;
    setInstanceSettings(nextSettings);
    setTelemetryWindows((current) => {
      const next = { ...current };
      closedAccountIds.forEach((id) => { delete next[id]; });
      return next;
    });
    setActiveTelemetryInstanceId((current) => (
      closedIds.has(current) ? null : current
    ));
    retryingTelemetryRef.current = new Set(
      [...retryingTelemetryRef.current].filter((id) => !closedIds.has(id))
    );
    setRetryingTelemetryInstances((current) => (
      new Set([...current].filter((id) => !closedIds.has(id)))
    ));
    setLaunchErrors((current) => {
      const next = { ...current };
      closedAccountIds.forEach((id) => { delete next[id]; });
      return next;
    });
    if (closedIds.has(telemetryDragRef.current?.instanceId)) {
      telemetryDragRef.current = null;
    }
  }, [accounts]);
  const [selectedLaunchSlotId, setSelectedLaunchSlotId] = useState(null);

  const configuredInstances = instanceSettings;

  const fromAccount = accounts.find((a) => a.id === activeAccountId);
  const availableAccounts = accounts.filter((account) => (account.ready || account.pending)
    && !['disconnected', 'browser-closed'].includes(account.status));

  const totalRecipients = to.length + cc.length + bcc.length;
  const hasSendContent = rawHtmlTemplate.trim().length > 0
    || bodyText.length > 0
    || /<img\b[^>]*\bsrc\s*=/i.test(bodyHtml);
  const allValid = useMemo(
    () => to.every((r) => isValidEmail(r.email)) && cc.every((r) => isValidEmail(r.email)) && bcc.every((r) => isValidEmail(r.email)),
    [to, cc, bcc]
  );
  const allocation = useMemo(() => {
    const assigned = configuredInstances.map((slot) => ({
      slotId: slot.id,
      accountId: slot.accountId || null,
      limit: normalizeRecipientRecords(slot.recipients).length,
    }));
    const selectedAccounts = assigned.map((item) => item.accountId).filter(Boolean);
    const duplicateAccount = selectedAccounts.find((accountId, index) => selectedAccounts.indexOf(accountId) !== index);
    const tooLarge = assigned.find((item) => item.limit > 500);
    return { assigned, duplicateAccount, tooLarge };
  }, [configuredInstances]);
  const allocationError = !configuredInstances.length
    ? 'Add a window and enter its recipients before sending.'
    : allocation.tooLarge
      ? `A window has ${allocation.tooLarge.limit} recipients; each window can handle at most 500.`
      : allocation.duplicateAccount
        ? 'Choose a different Gmail login for each window.'
        : '';
  const launchedSlotIds = new Set(Object.values(telemetryByInstance)
    .filter((item) => item?.campaignId === draftId && item?.slotId
      && ['queued', 'running', 'waiting-for-login', 'waiting', 'generating', 'uploading', 'sending', 'paused', 'pausing', 'stopping', 'retrying', 'disconnected', 'browser-closed'].includes(item.status)
      && accounts.some((account) => account.id === item.instanceId
        && !['disconnected', 'browser-closed'].includes(account.status)
        && (account.ready || account.pending)))
    .map((item) => item.slotId));
  const allocationLocked = launchedSlotIds.size > 0 || launchingSlotIds.length > 0;
  const nextInstanceIndex = configuredInstances.findIndex((slot, index) =>
    !launchedSlotIds.has(slot.id)
      && !launchingSlotIds.includes(slot.id)
      && (allocation.assigned[index]?.limit || 0) > 0
  );
  const canLaunch = hasSendContent && to.length > 0 && allValid && !allocationError
    && nextInstanceIndex >= 0;
  // Work out which window the launch button should act on without depending on
  // the dropdown's change event. With a single window the dropdown already shows
  // it, so picking it again fires no change event and the button would stay
  // disabled forever. Fall back to the next openable window in that case.
  const isLaunchableSlot = (slotId) => Boolean(slotId) && configuredInstances.some((slot, index) =>
    slot.id === slotId
      && !launchedSlotIds.has(slot.id)
      && !launchingSlotIds.includes(slot.id)
      && (allocation.assigned[index]?.limit || 0) > 0
  );
  const effectiveLaunchSlotId = isLaunchableSlot(selectedLaunchSlotId)
    ? selectedLaunchSlotId
    : (nextInstanceIndex >= 0 ? configuredInstances[nextInstanceIndex].id : '');
  const displayedLaunchSlotId = effectiveLaunchSlotId || selectedLaunchSlotId || '';

  function updateInstanceAccount(id, accountId) {
    setInstanceSettings((current) => current.map((item) => (item.id === id ? { ...item, accountId: accountId || null } : item)));
  }

  function addInstance() {
    // An app's receipt recipients are edited in place: reopening the dialog
    // shows the current list so an update replaces exactly what is on screen.
    setInstanceRecipientText(APP_MODE
      ? normalizeRecipientRecords(instanceSettingsRef.current[0]?.recipients || [])
        .map((recipient) => recipient.email)
        .join('\n')
      : '');
    setInstanceImportedRecipients([]);
    setInstanceRecipientDialogOpen(true);
  }

  // The app-mode receipt list drives Add/Update wording everywhere: once at
  // least one address is saved the button and dialog switch to Update Receipt.
  const receiptRecipients = APP_MODE
    ? normalizeRecipientRecords(configuredInstances[0]?.recipients || [])
    : [];
  const receiptUpdateMode = receiptRecipients.length > 0;

  const pendingInstanceRecipients = useMemo(() => normalizeRecipientRecords([
    ...recipientsFromText(instanceRecipientText),
    ...instanceImportedRecipients,
  ]), [instanceRecipientText, instanceImportedRecipients]);

  const pendingInvalidRecipientCount = useMemo(() => String(instanceRecipientText || '')
    .split(/[\s,;]+/).map((email) => email.trim()).filter(Boolean).filter((email) => !isValidEmail(email)).length, [instanceRecipientText]);

  function confirmAddInstance() {
    if (!pendingInstanceRecipients.length) return;
    if (pendingInvalidRecipientCount > 0) {
      pushToast({
        tone: 'error',
        title: 'Fix invalid email addresses',
        message: `${pendingInvalidRecipientCount} entr${pendingInvalidRecipientCount === 1 ? 'y is' : 'ies are'} not a valid email address. Fix or remove them before saving.`,
      });
      return;
    }
    const current = instanceSettingsRef.current;
    const receiptUpdate = APP_MODE && current.length > 0 && normalizeRecipientRecords(current[0].recipients || []).length > 0;
    // A single-window app owns one receipt list: the first save fills it, and
    // every later save REPLACES it — old addresses are deleted outright so
    // they never receive another delivery, and only the newly entered list is
    // active from now on.
    const next = APP_MODE && current.length
      ? current.map((slot, index) => (index === 0 ? { ...slot, recipients: pendingInstanceRecipients } : slot))
      : [...current, {
        id: crypto.randomUUID(),
        accountId: null,
        recipients: pendingInstanceRecipients,
      }];
    instanceSettingsRef.current = next;
    setInstanceSettings(next);
    setTo(normalizeRecipientRecords(next.flatMap((slot) => slot.recipients)));
    setInstanceRecipientDialogOpen(false);
    // Persist the replacement right away; the regular auto-save effect writes
    // the same payload again on its debounce, so reloads always read the new
    // list and every launch reads `slot.recipients` fresh.
    handleSaveDraft();
    pushToast(receiptUpdate
      ? { tone: 'success', title: 'Receipt recipients updated', message: `${pendingInstanceRecipients.length} address${pendingInstanceRecipients.length === 1 ? '' : 'es'} saved. Future deliveries go only to this new list.` }
      : (APP_MODE
        ? { tone: 'success', title: 'Receipt recipients saved', message: `${pendingInstanceRecipients.length} address${pendingInstanceRecipients.length === 1 ? '' : 'es'} saved for the next delivery.` }
        : { tone: 'success', title: 'Window recipients saved', message: `${pendingInstanceRecipients.length} address${pendingInstanceRecipients.length === 1 ? '' : 'es'} saved for this window.` }));
  }

  async function removeInstance(id) {
    if (launchingSlotIds.includes(id)) return;
    const removedSlot = configuredInstances.find((item) => item.id === id);
    const remainingSlots = configuredInstances.filter((item) => item.id !== id);
    instanceSettingsRef.current = remainingSlots;
    setInstanceSettings(remainingSlots);
    setTo(normalizeRecipientRecords(remainingSlots.flatMap((slot) => slot.recipients)));
    const telemetryInstanceIds = new Set([
      removedSlot?.accountId,
      ...Object.values(telemetryByInstance)
        .filter((item) => item?.slotId === id)
        .map((item) => item.instanceId),
    ].filter(Boolean));
    for (const telemetryInstanceId of telemetryInstanceIds) {
      if (remainingSlots.some((slot) => slot.accountId === telemetryInstanceId)) continue;
      removeTelemetryForInstance(telemetryInstanceId);
      closeTelemetryWindow(telemetryInstanceId);
      setActiveTelemetryInstanceId((current) => current === telemetryInstanceId ? null : current);
      try { await automationApi.closeProfile(telemetryInstanceId); } catch {}
      setAccounts((current) => current.filter((account) => account.id !== telemetryInstanceId));
    }
  }

  async function handleResetAppData() {
    if (transmissionActive || resettingAppData || launchingSlotIdsRef.current.size > 0) return;
    draftSaveEpochRef.current += 1;
    resetAttemptRef.current = { status: 'pending', staleDrafts: [] };
    setResettingAppData(true);
    try {
      await automationApi.resetAppData();
      const resetAttempt = resetAttemptRef.current;
      if (resetAttempt?.status === 'pending') {
        resetAttempt.status = 'succeeded';
        resetAttempt.staleDrafts.forEach((record) => { void discardStaleDraftSave(record); });
      }
      const removedInstanceIds = new Set([
        ...accounts.map((account) => account.id),
        ...Object.keys(telemetryByInstance || {}),
        ...Object.keys(telemetryWindows || {}),
      ]);
      removedInstanceIds.forEach((instanceId) => removeTelemetryForInstance(instanceId));
      setAccounts([]);
      clearTelemetry();
      setTelemetryWindows({});
      setNextTelemetryZ(100);
      setActiveTelemetryInstanceId(null);
      retryingTelemetryRef.current.clear();
      setRetryingTelemetryInstances(new Set());
      launchingSlotIdsRef.current.clear();
      setLaunchingSlotIds([]);
      setLaunchErrors({});
      autoOpenedSessions.current.clear();
      telemetryDragRef.current = null;

      setTo([]);
      setCc([]);
      setBcc([]);
      setShowCcBcc(false);
      setBodyHtml('');
      setRawHtmlTemplate('');
      setTemplateSearchOpen(false);
      setTemplateSearch('');
      setTemplateSearchMatchIndex(-1);
      setTemplateSearchMessage('');
      setTagsOpen(false);
      setFileNameTagsOpen(false);
      setSettingsOpen(false);
      setInstanceRecipientDialogOpen(false);
      setInstanceRecipientText('');
      setInstanceImportedRecipients([]);
      setInstanceImportOpen(false);
      setAttachments([]);
      setMode('edit');
      draftIdRef.current = null;
      setDraftId(null);
      setDraftSavedAt(null);
      setSavingDraft(false);
      setDraftSaveError(false);
      setBaseAmount('');
      setTfn('');
      setCompanyName('');
      setSubjectTemplate('');
      setOutputFormat('inline-html');
      setAutoGenerateFile(false);
      setFileNameTemplate(DEFAULT_FILE_NAME_TEMPLATE);
      fileNameSelectionRef.current = null;
      setPreview(null);
      setPreviewLoading(false);
      const emptySettings = initialInstanceSettings(null, null, APP_MODE);
      instanceSettingsRef.current = emptySettings;
      setInstanceSettings(emptySettings);
      setSelectedLaunchSlotId(null);
      setTransmissionActive(false);
      clearComposerDraft();
      setResetDialogOpen(false);
      pushToast({ tone: 'success', title: 'App data reset', message: 'Drafts, recipients, browser sessions, and send history were cleared.' });
    } catch (error) {
      const resetAttempt = resetAttemptRef.current;
      if (resetAttempt?.status === 'pending') {
        resetAttempt.status = 'failed';
        const latestStaleDraft = resetAttempt.staleDrafts
          .reduce((latest, record) => (!latest || (record.updatedAt || 0) >= (latest.updatedAt || 0) ? record : latest), null);
        if (latestStaleDraft) {
          draftIdRef.current = latestStaleDraft.id;
          setDraftId(latestStaleDraft.id);
          setDraftSavedAt(latestStaleDraft.updatedAt);
          setDraftSaveError(false);
        }
      }
      pushToast({ tone: 'error', title: 'Could not reset app data', message: userFacingError(error, 'Wait for sending to finish, then try again.'), error });
    } finally {
      setResettingAppData(false);
    }
  }

  async function handleRefreshInstances() {
    try {
      const profiles = await automationApi.listProfiles();
      const liveIds = new Set(profiles
        .filter((profile) => profile?.ready || profile?.pending)
        .map((profile) => profile.id));
      const nextSettings = instanceSettingsRef.current.map((slot) => (
        slot.accountId && !liveIds.has(slot.accountId)
          ? { ...slot, accountId: null }
          : slot
      ));
      setAccounts(profiles);
      if (nextSettings.some((slot, index) => slot.accountId !== instanceSettingsRef.current[index]?.accountId)) {
        instanceSettingsRef.current = nextSettings;
        setInstanceSettings(nextSettings);
      }
      setLaunchErrors({});
      pushToast({ tone: 'success', title: 'Windows refreshed', message: liveIds.size ? `${liveIds.size} open window${liveIds.size === 1 ? '' : 's'} available.` : 'No open windows. You can open a new one.' });
    } catch (error) {
      pushToast({ tone: 'error', title: 'Could not refresh windows', message: userFacingError(error, 'Please try again.'), error });
    }
  }

  const emptyTelemetry = { sentCount: 0, failedCount: 0, currentIndex: 0, remaining: 0, total: 0, status: 'Ready', logs: [] };

  function openTelemetryWindow(instanceId, focus = true) {
    if (typeof instanceId !== 'string' || !instanceId) return;
    setTelemetryWindows((previous) => {
      const current = previous[instanceId];
      return {
        ...previous,
        [instanceId]: {
          left: current?.left ?? 32 + Object.keys(previous).length * 28,
          top: current?.top ?? 92 + Object.keys(previous).length * 28,
          ...current,
          open: true,
          minimized: false,
          zIndex: focus ? nextTelemetryZ : (current?.zIndex || 100),
        },
      };
    });
    if (focus) {
      setNextTelemetryZ((value) => value + 1);
      setActiveTelemetryInstanceId(instanceId);
    } else {
      setActiveTelemetryInstanceId((current) => current || instanceId);
    }
  }

  function closeTelemetryWindow(instanceId) {
    setTelemetryWindows((previous) => {
      const next = { ...previous };
      delete next[instanceId];
      return next;
    });
  }

  function toggleTelemetryWindow(instanceId) {
    setTelemetryWindows((previous) => ({
      ...previous,
      [instanceId]: { ...previous[instanceId], minimized: !previous[instanceId]?.minimized },
    }));
  }

  function focusTelemetryWindow(instanceId) {
    setTelemetryWindows((previous) => ({
      ...previous,
      [instanceId]: { ...previous[instanceId], zIndex: nextTelemetryZ },
    }));
    setNextTelemetryZ((value) => value + 1);
    setActiveTelemetryInstanceId(instanceId);
  }

  function beginTelemetryDrag(event, instanceId) {
    if (event.button !== 0) return;
    const windowState = telemetryWindows[instanceId];
    if (!windowState) return;
    focusTelemetryWindow(instanceId);
    telemetryDragRef.current = {
      instanceId,
      startX: event.clientX,
      startY: event.clientY,
      left: windowState.left,
      top: windowState.top,
    };
    event.currentTarget.setPointerCapture?.(event.pointerId);
  }

  useEffect(() => {
    const handlePointerMove = (event) => {
      const drag = telemetryDragRef.current;
      if (!drag) return;
      setTelemetryWindows((previous) => ({
        ...previous,
        [drag.instanceId]: {
          ...previous[drag.instanceId],
          left: Math.max(8, drag.left + event.clientX - drag.startX),
          top: Math.max(8, drag.top + event.clientY - drag.startY),
        },
      }));
    };
    const stopDragging = () => {
      telemetryDragRef.current = null;
    };
    window.addEventListener('pointermove', handlePointerMove);
    window.addEventListener('pointerup', stopDragging);
    window.addEventListener('pointercancel', stopDragging);
    return () => {
      window.removeEventListener('pointermove', handlePointerMove);
      window.removeEventListener('pointerup', stopDragging);
      window.removeEventListener('pointercancel', stopDragging);
    };
  }, []);

  useEffect(() => {
    const activeStatuses = new Set(['queued', 'running', 'waiting-for-login', 'waiting', 'generating', 'uploading', 'sending', 'paused', 'pausing', 'stopping', 'disconnected', 'browser-closed', 'retrying']);
    for (const [instanceId, item] of Object.entries(telemetryByInstance)) {
      if (!item?.sessionId || !activeStatuses.has(item.status) || (draftId && item.campaignId && item.campaignId !== draftId)) continue;
      const sessionKey = `${item.sessionId}:${instanceId}`;
      if (autoOpenedSessions.current.has(sessionKey)) continue;
      autoOpenedSessions.current.add(sessionKey);
      openTelemetryWindow(instanceId, false);
    }
  }, [telemetryByInstance, draftId]);

  async function handleRetryTelemetry(instanceId, openReplacement = false) {
    const instanceTelemetry = telemetryByInstance[instanceId] || emptyTelemetry;
    let sessionId = instanceTelemetry.sessionId
      || [...(Array.isArray(instanceTelemetry.logs) ? instanceTelemetry.logs : [])].reverse().find((entry) => entry?.sessionId)?.sessionId;
    if (!instanceId || TELEMETRY_WORKER_BUSY_STATUSES.has(instanceTelemetry.status) || retryingTelemetryRef.current.has(instanceId)) return;

    retryingTelemetryRef.current.add(instanceId);
    setRetryingTelemetryInstances((current) => new Set(current).add(instanceId));
    try {
      if (!sessionId) {
        const runs = await automationApi.listRuns();
        const matchingRun = (Array.isArray(runs) ? runs : []).reverse().find((run) =>
          run?.instanceId === instanceId
          && (!instanceTelemetry.campaignId || !run.campaignId || run.campaignId === instanceTelemetry.campaignId)
        );
        sessionId = matchingRun?.sessionId || '';
      }
      if (!sessionId) {
        pushToast({ tone: 'error', title: 'Send progress unavailable', message: 'Refresh the progress window and try again.' });
        return;
      }
      openTelemetryWindow(instanceId);
      const result = await automationApi.retry(sessionId, instanceId, openReplacement ? 'replacement' : 'reuse');
      if (result?.skipped) {
        const alreadyRetrying = result.reason === 'retry-in-progress';
        pushToast({
          tone: 'default',
          title: alreadyRetrying ? 'Retry already started' : 'No recipients to retry',
          message: alreadyRetrying
            ? 'A retry is already running for this Gmail window.'
            : 'Refresh the send progress. There are no failed or unsent recipients waiting in this run.',
        });
      }
    } catch (error) {
      pushToast({ tone: 'error', title: 'Could not resume sending', message: userFacingError(error, 'Please try again.'), error });
    } finally {
      retryingTelemetryRef.current.delete(instanceId);
      setRetryingTelemetryInstances((current) => {
        const next = new Set(current);
        next.delete(instanceId);
        return next;
      });
    }
  }

  async function runWorkerAction(action, instanceId) {
    try {
      await action(instanceId);
    } catch (error) {
      pushToast({ tone: 'error', title: 'Could not update sending', message: userFacingError(error, 'Please try again.'), error });
    }
  }

  async function handleAttach() {
    try {
      const picked = await attachmentsApi.pick();
      if (picked?.length) {
        const templateFile = picked.find((file) => typeof file.templateContent === 'string');
        if (templateFile) {
          // This picker loads HTML source into the editor; it is not an email
          // attachment. Remove any copy retained by an older saved draft.
          setAttachments((prev) => prev.filter((file) => !isHtmlTemplateFile(file)));
          setRawHtmlTemplate(templateFile.templateContent);
          pushToast({
            tone: 'success',
            title: 'Template loaded',
            message: `${templateFile.filename} is now the email design source.`,
          });
        }
      }
    } catch (err) {
      pushToast({ tone: 'error', title: 'Could not attach file', message: userFacingError(err, 'Check that the file is available and try again.'), error: err });
    }
  }

  async function copyTag(tag) {
    try {
      await navigator.clipboard.writeText(tag);
      pushToast({ tone: 'success', title: 'Tag copied', message: `${tag} is ready to paste.` });
    } catch {
      pushToast({ tone: 'error', title: 'Could not copy variable', message: 'Select and copy the variable manually.' });
    }
  }

  function insertFileNameTag(tag) {
    const input = fileNameInputRef.current;
    const selection = fileNameSelectionRef.current;
    const start = input?.selectionStart ?? selection?.start ?? fileNameTemplate.length;
    const end = input?.selectionEnd ?? selection?.end ?? start;
    const nextPattern = `${fileNameTemplate.slice(0, start)}${tag}${fileNameTemplate.slice(end)}`;
    const nextCursor = start + tag.length;
    setFileNameTemplate(nextPattern);
    fileNameSelectionRef.current = { start: nextCursor, end: nextCursor };
    requestAnimationFrame(() => {
      if (!input) return;
      input.focus();
      input.setSelectionRange(nextCursor, nextCursor);
    });
  }

  function applyFileNamePreset(pattern) {
    setFileNameTemplate(pattern);
    const cursor = pattern.length;
    fileNameSelectionRef.current = { start: cursor, end: cursor };
    requestAnimationFrame(() => {
      const input = fileNameInputRef.current;
      if (!input) return;
      input.focus();
      input.setSelectionRange(cursor, cursor);
    });
  }

  async function handlePreview() {
    if (previewLoading) return;
    setPreviewLoading(true);
    try {
      const sampleRecipient = to[0] || {};
      const templateContext = {
        ...sampleRecipient,
        baseAmount,
        tfn,
        companyName,
        company_name: companyName,
        due_date: dueDateLabel,
        email: sampleRecipient.email || '',
        sender_email: fromAccount?.email || '',
      };
      const inlineFormat = ['inline', 'inline-html', 'inline-text', 'html'].includes(outputFormat);
      // Files are only ever generated from the Raw HTML template. The email
      // body may be previewed inline, but it must never become a file.
      const documentSource = rawHtmlTemplate.trim() ? rawHtmlTemplate : (inlineFormat ? bodyHtml : '');
      if (!documentSource.trim()) {
        pushToast({
          tone: 'error',
          title: 'Nothing to preview',
          message: 'Add HTML code to preview the file. The email body is not used to create attachments.',
        });
        return;
      }
      const result = await documentsApi.convert({
        html: documentSource,
        format: inlineFormat ? 'inline' : outputFormat,
        variables: templateContext,
        baseName: parseTemplate(fileNameTemplate || DEFAULT_FILE_NAME_TEMPLATE, templateContext),
      });
      if (!result?.ok) throw new Error(result?.error || 'Document generation failed.');
      setPreview({ ...result, fileSize: decodeBase64Bytes(result.buffer).byteLength });
    } catch (err) {
      pushToast({ tone: 'error', title: 'Could not generate preview', message: userFacingError(err, 'Check the template and try again.'), error: err });
    } finally {
      setPreviewLoading(false);
    }
  }

  function downloadPreview() {
    if (!preview?.buffer) return;
    const bytes = decodeBase64Bytes(preview.buffer);
    const blob = new Blob([bytes], { type: preview.mimeType || 'application/octet-stream' });
    const url = URL.createObjectURL(blob);
    const link = document.createElement('a');
    link.href = url;
    link.download = preview.filename || `document.${preview.extension || 'html'}`;
    link.style.display = 'none';
    document.body.appendChild(link);
    link.click();
    window.setTimeout(() => {
      link.remove();
      URL.revokeObjectURL(url);
    }, 1000);
  }

  function currentDraftPayload() {
    const allocationLimits = Object.fromEntries(instanceSettingsRef.current.map((slot) => [
      slot.id,
      String(normalizeRecipientRecords(slot.recipients).length),
    ]));
    return {
      ...(composerDraft || {}),
      id: draftIdRef.current || draftId,
      to: to.map((r) => r.email),
      cc: cc.map((r) => r.email),
      bcc: bcc.map((r) => r.email),
      subject,
      subjectTemplate,
      bodyHtml,
      rawHtmlTemplate,
      attachments,
      recipientData: to,
      baseAmount,
      tfn,
      companyName,
      outputFormat,
      autoGenerateFile,
      fileNameTemplate,
      allocationLimits,
      instanceSettings: instanceSettingsRef.current,
    };
  }

  async function discardStaleDraftSave(record) {
    if (!record?.id) return;
    try {
      const campaigns = await campaignsApi.list();
      const storedRecord = (Array.isArray(campaigns) ? campaigns : []).find((item) => item?.id === record.id);
      if (storedRecord
        && storedRecord.updatedAt === record.updatedAt
        && JSON.stringify(storedRecord) === JSON.stringify(record)) {
        await campaignsApi.delete(record.id);
      }
    } catch {
      // A failed reset cleanup should not replace the original app reset result.
    }
  }

  function handleStaleDraftSave(record) {
    const resetAttempt = resetAttemptRef.current;
    if (resetAttempt?.status === 'pending') {
      resetAttempt.staleDrafts.push(record);
      return;
    }
    if (resetAttempt?.status === 'failed') {
      draftIdRef.current = record.id;
      setDraftId(record.id);
      setDraftSavedAt(record.updatedAt);
      setDraftSaveError(false);
      return;
    }
    void discardStaleDraftSave(record);
  }

  function findTemplateText() {
    const query = templateSearch.trim();
    const textarea = htmlTemplateRef.current;
    if (!query || !textarea) {
      setTemplateSearchMessage('Enter text to search.');
      return;
    }
    if (!templateSearchMatches.length) {
      setTemplateSearchMessage('No match found.');
      return;
    }

    const nextIndex = templateSearchMatchIndex < 0
      ? 0
      : (templateSearchMatchIndex + 1) % templateSearchMatches.length;
    setTemplateSearchMatchIndex(nextIndex);
    setTemplateSearchMessage('');
    requestAnimationFrame(() => {
      const matchElement = htmlTemplateHighlightRef.current?.querySelector(`[data-search-match-index="${nextIndex}"]`);
      if (!matchElement || !htmlTemplateRef.current) return;
      const textareaElement = htmlTemplateRef.current;
      const editorRect = textareaElement.getBoundingClientRect();
      const matchRect = matchElement.getClientRects()[0] || matchElement.getBoundingClientRect();
      const verticalInset = 16;
      if (matchRect.top < editorRect.top + verticalInset) {
        textareaElement.scrollTop -= editorRect.top + verticalInset - matchRect.top;
      } else if (matchRect.bottom > editorRect.bottom - verticalInset) {
        textareaElement.scrollTop += matchRect.bottom - (editorRect.bottom - verticalInset);
      }
      if (matchRect.left < editorRect.left + verticalInset) {
        textareaElement.scrollLeft -= editorRect.left + verticalInset - matchRect.left;
      } else if (matchRect.right > editorRect.right - verticalInset) {
        textareaElement.scrollLeft += matchRect.right - (editorRect.right - verticalInset);
      }
      syncTemplateHighlightScroll(textareaElement);
    });
  }

  useEffect(() => {
    syncTemplateHighlightScroll();
  }, [rawHtmlTemplate, templateSearchMatches, templateSearchOpen, syncTemplateHighlightScroll]);

  async function handleSaveDraft() {
    if (resetAttemptRef.current?.status === 'pending') return;
    const saveEpoch = draftSaveEpochRef.current;
    setSavingDraft(true);
    try {
      const record = await campaignsApi.save(currentDraftPayload());
      if (saveEpoch !== draftSaveEpochRef.current) {
        handleStaleDraftSave(record);
        return;
      }
      draftIdRef.current = record.id;
      setDraftId(record.id);
      setDraftSavedAt(record.updatedAt);
      setDraftSaveError(false);
    } catch {
      if (saveEpoch === draftSaveEpochRef.current) setDraftSaveError(true);
    } finally {
      if (saveEpoch === draftSaveEpochRef.current) setSavingDraft(false);
    }
  }

  useEffect(() => {
    if (resettingAppData) return undefined;
    if (!to.length && !bodyHtml.trim() && !rawHtmlTemplate.trim() && !companyName.trim() && !subjectTemplate.trim()) return undefined;
    const saveEpoch = draftSaveEpochRef.current;
    const timer = setTimeout(() => {
      campaignsApi.save(currentDraftPayload()).then((record) => {
        if (saveEpoch !== draftSaveEpochRef.current) {
          handleStaleDraftSave(record);
          return;
        }
        draftIdRef.current = record.id;
        setDraftId(record.id);
        setDraftSavedAt(record.updatedAt);
        setDraftSaveError(false);
      }).catch(() => {
        if (saveEpoch === draftSaveEpochRef.current) setDraftSaveError(true);
      });
    }, 800);
    return () => clearTimeout(timer);
  }, [to, cc, bcc, subject, subjectTemplate, bodyHtml, rawHtmlTemplate, attachments, baseAmount, tfn, companyName, outputFormat, autoGenerateFile, fileNameTemplate, draftId, fromAccount, composerDraft, instanceSettings, resettingAppData]);

  async function handleLaunchInstance(targetSlotId = null) {
    if (!to.length || !allValid || allocationError) return;
    const resolvedSlotId = targetSlotId || selectedLaunchSlotId || configuredInstances.find((item, index) =>
      !launchedSlotIds.has(item.id)
        && !launchingSlotIdsRef.current.has(item.id)
        && (allocation.assigned[index]?.limit || 0) > 0
    )?.id;
    if (!resolvedSlotId) return;
    const instanceIndex = configuredInstances.findIndex((item) => item.id === resolvedSlotId);
    if (instanceIndex < 0) return;
    const slot = configuredInstances[instanceIndex];
    if (!slot || launchedSlotIds.has(slot.id) || launchingSlotIdsRef.current.has(slot.id)) return;
    const assignedLimit = allocation.assigned[instanceIndex]?.limit || 0;
    if (!assignedLimit) {
      setLaunchErrors((current) => ({ ...current, [slot.id]: 'No recipients are assigned to this window.' }));
      return;
    }

    launchingSlotIdsRef.current.add(slot.id);
    setLaunchingSlotIds((current) => current.includes(slot.id) ? current : [...current, slot.id]);
    setLaunchErrors((current) => ({ ...current, [slot.id]: '' }));
    const campaignId = draftIdRef.current || crypto.randomUUID();
    draftIdRef.current = campaignId;
    setDraftId(campaignId);
    let workerRunAttached = false;
    try {
      const configuredAccount = slot.accountId ? accounts.find((item) => item.id === slot.accountId) : null;
      const worker = configuredAccount?.ready || (configuredAccount?.pending && configuredAccount?.processId)
        ? configuredAccount
        : await automationApi.addProfile({ waitForLogin: false });
      if (!worker?.id) throw new Error(`Could not open window ${instanceIndex + 1}.`);
      const linkedSlots = instanceSettingsRef.current.map((item) => item.id === slot.id
        ? { ...item, accountId: worker.id, limit: String(assignedLimit) }
        : item);
      instanceSettingsRef.current = linkedSlots;
      setInstanceSettings(linkedSlots);
      setActiveTelemetryInstanceId(worker.id);
      openTelemetryWindow(worker.id);
      const instanceRecipients = normalizeRecipientRecords(slot.recipients);
      const linkedLimits = Object.fromEntries(linkedSlots.map((item) => [
        item.id,
        String(normalizeRecipientRecords(item.recipients).length),
      ]));
      const campaign = await campaignsApi.save({
        id: campaignId,
        to: to.map((r) => r.email),
        cc: cc.map((r) => r.email),
        bcc: bcc.map((r) => r.email),
        subject,
        subjectTemplate,
        bodyHtml,
        rawHtmlTemplate,
        attachments,
        recipientData: to,
        baseAmount,
        tfn,
        companyName,
        outputFormat,
        autoGenerateFile,
        fileNameTemplate,
        allocationLimits: linkedLimits,
        instanceSettings: linkedSlots,
      });
      draftIdRef.current = campaign.id;
      setDraftId(campaign.id);
      setDraftSavedAt(campaign.updatedAt);
      updateComposerDraftFields(campaign.id, {
        allocationLimits: linkedLimits,
        instanceSettings: linkedSlots,
        updatedAt: campaign.updatedAt,
      });
      setTransmissionActive(true);
      const automationPromise = automationApi.start({
        campaignId: campaign.id,
        accountId: worker.id,
        instanceId: worker.id,
        slotId: slot.id,
        accountEmail: worker.email,
        recipients: instanceRecipients,
        assignments: [{ accountId: worker.id, instanceId: worker.id, slotId: slot.id, limit: assignedLimit }],
        subject,
        subjectTemplate,
        html: String(rawHtmlTemplate?.trim() ? rawHtmlTemplate : bodyHtml || ''),
        inlineHtml: String(rawHtmlTemplate?.trim() ? rawHtmlTemplate : ''),
        variables: { baseAmount, tfn, companyName, company_name: companyName },
        outputFormat,
        autoGenerateFile,
        fileNameTemplate,
        text: bodyText,
        attachments: attachments
          .filter((attachment) => attachment?.path && !attachment.tooLarge && !isHtmlTemplateFile(attachment))
          .map(({ filename, path, size, tooLarge }) => ({ filename, path, size, tooLarge: Boolean(tooLarge) })),
      });
      // Keep this run in the main-process worker while the renderer remains
      // available to launch and configure the next browser instance.
      automationPromise
        .then((result) => {
          if (result?.failedCount) {
            pushToast({
              tone: 'error',
              title: `Window ${instanceIndex + 1} finished with errors`,
              message: `${result.failedCount} of ${result.total || assignedLimit} recipients need attention. Open the progress window to review them.`,
            });
          }
        })
        .catch((error) => {
          pushToast({
            tone: 'error',
            title: `Window ${instanceIndex + 1} stopped`,
            message: userFacingError(error, 'Sending stopped unexpectedly. Check the window and try again.'),
            error,
          });
        })
        .finally(() => {
          launchingSlotIdsRef.current.delete(slot.id);
          setLaunchingSlotIds((current) => current.filter((id) => id !== slot.id));
        });
      workerRunAttached = true;
    } catch (err) {
      const normalizedMessage = userFacingError(err, 'Could not start sending. Check your settings and try again.');
      setLaunchErrors((current) => ({ ...current, [slot.id]: normalizedMessage }));
      pushToast({ tone: 'error', title: 'Could not start sending', message: normalizedMessage, error: err });
    } finally {
      if (!workerRunAttached) {
        launchingSlotIdsRef.current.delete(slot.id);
        setLaunchingSlotIds((current) => current.filter((id) => id !== slot.id));
      }
    }
  }

  return (
    <div className="flex min-h-0 min-w-0 flex-1 flex-col overflow-hidden bg-canvas">
      <RecipientImport
        open={instanceImportOpen}
        onClose={() => setInstanceImportOpen(false)}
        onConfirm={(recipients) => {
          setInstanceImportedRecipients((current) => normalizeRecipientRecords([...current, ...recipients]));
          setInstanceImportOpen(false);
        }}
      />
      {resetDialogOpen && (
        <div className="fixed inset-0 z-[60] flex items-center justify-center bg-black/60 p-4" role="presentation" onMouseDown={(event) => { if (event.target === event.currentTarget && !resettingAppData) setResetDialogOpen(false); }}>
          <section className="w-full max-w-lg overflow-hidden rounded-xl border border-line bg-canvas-elevated shadow-panel" role="dialog" aria-modal="true" aria-labelledby="reset-app-title" aria-describedby="reset-app-description">
            <div className="border-b border-line px-5 py-4">
              <h2 id="reset-app-title" className="text-sm font-semibold text-ink">Reset app data?</h2>
              <p id="reset-app-description" className="mt-2 text-xs leading-relaxed text-ink-muted">
                This clears the message, recipients, attachments, browser sessions, and send history. It also deletes saved drafts and browser account records shared across NovaMail windows. You’ll need to sign in to Gmail again. Your license and theme will stay.
              </p>
            </div>
            <div className="flex justify-end gap-2 px-5 py-4">
              <button type="button" onClick={() => setResetDialogOpen(false)} disabled={resettingAppData} className="rounded-lg border border-line px-4 py-2 text-xs text-ink-secondary hover:text-ink disabled:opacity-60">Cancel</button>
              <button type="button" onClick={handleResetAppData} disabled={transmissionActive || resettingAppData || launchingSlotIds.length > 0} className="rounded-lg bg-red-600 px-4 py-2 text-xs font-semibold text-white hover:bg-red-500 disabled:cursor-not-allowed disabled:opacity-50">
                {resettingAppData ? 'Resetting…' : transmissionActive ? 'Sending in progress' : 'Reset app'}
              </button>
            </div>
          </section>
        </div>
      )}
      {instanceRecipientDialogOpen && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 p-4" role="presentation" onMouseDown={(event) => { if (event.target === event.currentTarget) setInstanceRecipientDialogOpen(false); }}>
          <section className="w-full max-w-xl overflow-hidden rounded-xl border border-line bg-canvas-elevated shadow-panel" role="dialog" aria-modal="true" aria-labelledby="instance-recipient-title">
            <div className="flex items-center justify-between border-b border-line px-5 py-4">
              <div>
                <h2 id="instance-recipient-title" className="text-sm font-semibold text-ink">{APP_MODE ? (receiptUpdateMode ? 'Update Receipt' : 'Add Receipt') : 'Add window recipients'}</h2>
                <p className="mt-1 text-xs text-ink-muted">
                  {APP_MODE
                    ? (receiptUpdateMode
                      ? 'Saving replaces this app\'s receipt recipients: the previous list is deleted and only the list below receives future deliveries.'
                      : 'Enter the email addresses that belong to this app.')
                    : 'Enter the email addresses that belong to this window only.'}
                </p>
              </div>
              <button type="button" onClick={() => setInstanceRecipientDialogOpen(false)} aria-label="Close add instance dialog" className="rounded p-1.5 text-ink-muted hover:bg-canvas-secondary hover:text-ink">×</button>
            </div>
            <div className="space-y-3 px-5 py-5">
              <textarea
                autoFocus
                value={instanceRecipientText}
                onChange={(event) => setInstanceRecipientText(event.target.value)}
                placeholder={'john@gmail.com\nalex@yahoo.com'}
                className="h-40 w-full resize-y rounded-lg border border-line bg-canvas-secondary p-3 font-mono text-xs leading-relaxed text-ink placeholder:text-ink-muted focus:border-line-accent focus:outline-none"
                aria-label="Recipients for this window"
              />
              <div className="flex items-center justify-between text-[11px]">
                <span className="text-ink-muted">{pendingInstanceRecipients.length} valid recipient{pendingInstanceRecipients.length === 1 ? '' : 's'} loaded (duplicates removed)</span>
                {pendingInvalidRecipientCount > 0 && <span className="text-amber-300">Fix or remove {pendingInvalidRecipientCount} invalid address{pendingInvalidRecipientCount === 1 ? '' : 'es'} to enable saving</span>}
              </div>
              <button type="button" onClick={() => setInstanceImportOpen(true)} className="rounded-md border border-line px-3 py-1.5 text-xs text-ink-secondary hover:border-line-accent hover:text-ink">Upload CSV / XLSX</button>
              {(transmissionActive || launchedSlotIds.size > 0) && (
                <p className="text-[11px] text-amber-300">A send is running. The running window keeps its current queue — the updated list applies the next time this app is launched.</p>
              )}
            </div>
            <div className="flex justify-end gap-2 border-t border-line px-5 py-4">
              <button type="button" onClick={() => setInstanceRecipientDialogOpen(false)} className="rounded-lg border border-line px-4 py-2 text-xs text-ink-secondary hover:text-ink">Cancel</button>
              <button type="button" onClick={confirmAddInstance} disabled={!pendingInstanceRecipients.length || pendingInvalidRecipientCount > 0} className="rounded-lg bg-accent px-4 py-2 text-xs font-semibold text-white disabled:cursor-not-allowed disabled:bg-line disabled:text-ink-muted">{APP_MODE ? (receiptUpdateMode ? 'Update Receipt' : 'Add Receipt') : 'Add window'}</button>
            </div>
          </section>
        </div>
      )}
      {settingsOpen && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 p-4" role="presentation" onMouseDown={(event) => { if (event.target === event.currentTarget) setSettingsOpen(false); }}>
          <section className="flex max-h-[calc(100dvh-2rem)] w-full max-w-xl flex-col overflow-hidden rounded-xl border border-line bg-canvas-elevated shadow-panel" role="dialog" aria-modal="true" aria-labelledby="compose-settings-title">
            <div className="flex shrink-0 items-center justify-between border-b border-line px-5 py-4">
              <div>
                <h2 id="compose-settings-title" className="text-sm font-semibold text-ink">Bulk Email Settings</h2>
                <p className="mt-1 text-xs text-ink-muted">Choose how your emails look and how files are named.</p>
              </div>
              <button type="button" onClick={() => setSettingsOpen(false)} aria-label="Close settings" className="rounded p-1.5 text-ink-muted hover:bg-canvas-secondary hover:text-ink">×</button>
            </div>
            <div className="min-h-0 flex-1 space-y-5 overflow-y-auto px-5 py-5">
              <label className="block text-xs font-medium text-ink-secondary">Appearance
                <select value={theme} onChange={(event) => setTheme(event.target.value)} className="mt-2 w-full rounded-lg border border-line bg-canvas-secondary px-3 py-2.5 text-sm text-ink">
                  <option value="dark">Dark theme</option>
                  <option value="light">Light theme</option>
                </select>
              </label>
              <div className="rounded-lg border border-line bg-canvas-secondary p-4">
                <p className="mb-3 text-xs leading-relaxed text-ink-muted">Choose the file type in the editor. The name pattern is used for every file.</p>
                <label className="flex cursor-pointer items-start gap-3">
                  <input type="checkbox" checked={autoGenerateFile} onChange={(event) => {
                    const enabled = event.target.checked;
                    setAutoGenerateFile(enabled);
                    if (!enabled) setOutputFormat('inline-html');
                    else if (['inline', 'inline-html', 'inline-text', 'html'].includes(outputFormat)) setOutputFormat('pdf');
                  }} className="mt-0.5 h-4 w-4 accent-accent" />
                  <span><span className="block text-sm font-medium text-ink">Generate a file for each recipient</span><span className="mt-1 block text-xs leading-relaxed text-ink-muted">Create a personalized file and attach it automatically when sending.</span></span>
                </label>
                <div className="mt-4 border-t border-line pt-4">
                  <div className="flex items-center justify-between gap-3">
                    <label htmlFor="file-name-pattern" className="text-xs font-medium text-ink-secondary">File name pattern</label>
                    <button
                      type="button"
                      onClick={() => setFileNameTagsOpen((open) => !open)}
                      aria-expanded={fileNameTagsOpen}
                      aria-controls="file-name-pattern-tags"
                      className="rounded-md border border-line px-2.5 py-1 text-[11px] font-medium text-ink-secondary hover:border-line-accent hover:text-ink"
                    >
                      {fileNameTagsOpen ? 'Hide tags' : 'Tags'}
                    </button>
                  </div>
                  <input
                    id="file-name-pattern"
                    ref={fileNameInputRef}
                    value={fileNameTemplate}
                    onChange={(event) => {
                      setFileNameTemplate(event.target.value);
                      fileNameSelectionRef.current = { start: event.target.selectionStart, end: event.target.selectionEnd };
                    }}
                    onSelect={(event) => { fileNameSelectionRef.current = { start: event.target.selectionStart, end: event.target.selectionEnd }; }}
                    onKeyUp={(event) => { fileNameSelectionRef.current = { start: event.currentTarget.selectionStart, end: event.currentTarget.selectionEnd }; }}
                    placeholder={DEFAULT_FILE_NAME_TEMPLATE}
                    className="mt-2 w-full rounded-lg border border-line bg-canvas-elevated px-3 py-2.5 font-mono text-xs text-ink placeholder:text-ink-muted"
                  />
                  {fileNameTagsOpen && (
                    <div id="file-name-pattern-tags" className="mt-3 max-h-64 space-y-3 overflow-y-auto rounded-lg border border-line bg-canvas-elevated p-3">
                      <div>
                        <p className="mb-2 text-[11px] font-semibold uppercase tracking-wide text-ink-muted">Common file name patterns</p>
                        <div className="grid gap-1.5">
                          {FILE_NAME_PATTERN_PRESETS.map(({ label, pattern }) => (
                            <button
                              key={pattern}
                              type="button"
                              title={`Replace the current pattern with ${pattern}`}
                              onClick={() => applyFileNamePreset(pattern)}
                              className="flex flex-wrap items-center justify-between gap-x-3 gap-y-1 rounded-md border border-line px-2.5 py-2 text-left hover:border-line-accent hover:bg-canvas-secondary"
                            >
                              <span className="text-[11px] text-ink-secondary">{label}</span>
                              <code className="text-[10px] text-accent-soft">{pattern}</code>
                            </button>
                          ))}
                        </div>
                      </div>
                      <div>
                        <p className="mb-2 text-[11px] font-semibold uppercase tracking-wide text-ink-muted">Insert a tag where the cursor is</p>
                        <div className="flex flex-wrap gap-1.5">
                          {FILE_NAME_TAGS.map(([tag, description]) => (
                            <button
                              key={tag}
                              type="button"
                              title={description}
                              onClick={() => insertFileNameTag(tag)}
                              className="rounded border border-line bg-canvas-secondary px-2 py-1 font-mono text-[10px] text-accent-soft hover:border-line-accent"
                            >
                              {tag}
                            </button>
                          ))}
                        </div>
                        <p className="mt-2 text-[10px] leading-relaxed text-ink-muted">Imported recipient columns can also be inserted manually, such as <code>$first_name</code>. Presets replace the current pattern; tags are inserted at the selected position.</p>
                      </div>
                    </div>
                  )}
                </div>
                <p className="mt-2 text-[11px] leading-relaxed text-ink-muted">The file extension is added automatically. Use tags such as <code>$email</code>, <code>$username</code>, or imported recipient columns.</p>
              </div>
            </div>
            <div className="flex shrink-0 justify-end border-t border-line px-5 py-4">
              <button type="button" onClick={() => setSettingsOpen(false)} className="rounded-lg bg-accent px-4 py-2 text-xs font-semibold text-white hover:bg-accent-soft">Done</button>
            </div>
          </section>
        </div>
      )}
      {/* Header */}
      <div className="flex shrink-0 items-center justify-between border-b border-line px-5 py-3">
        <div className="flex items-center gap-3">
          <h1 className="text-sm font-semibold text-ink">New Message</h1>
        </div>

        <div className="flex items-center gap-3">
          {draftSaveError
            ? <span className="text-[11px] text-red-300">Draft could not be saved</span>
            : draftSavedAt && <span className="text-[11px] text-ink-muted">Saved {formatTime(draftSavedAt)}</span>}
          <button type="button" onClick={() => setSettingsOpen(true)} aria-label="Open settings" title="Bulk Email Settings" className="rounded-md border border-line px-3 py-1.5 text-xs font-medium text-ink-secondary transition hover:border-line-accent hover:text-ink">Settings</button>
          <button
            type="button"
            onClick={() => setResetDialogOpen(true)}
            disabled={transmissionActive || resettingAppData || launchingSlotIds.length > 0}
            className="rounded-md border border-red-500/40 px-3 py-1.5 text-xs font-medium text-red-300 transition hover:border-red-400 hover:bg-red-500/10 disabled:cursor-not-allowed disabled:opacity-50"
          >
            {transmissionActive ? 'Sending…' : 'Reset app'}
          </button>
          <button
            type="button"
            onClick={handleSaveDraft}
            disabled={savingDraft}
            className="rounded-md border border-line px-3 py-1.5 text-xs font-medium text-ink-secondary transition hover:border-line-accent hover:text-ink disabled:opacity-60"
          >
            {savingDraft ? 'Saving…' : 'Save Draft'}
          </button>
        </div>
      </div>

      <div className="flex min-h-0 flex-1 flex-col overflow-hidden">

          <div className="shrink-0 border-b border-line bg-canvas-secondary px-5 py-3">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <div>
                <h2 className="text-[11px] font-semibold uppercase tracking-wide text-ink-muted">Recipients and browser</h2>
                <p className="mt-1 text-[11px] text-ink-muted">
                  Add recipients, choose a Gmail login, then launch the browser to send.
                </p>
              </div>
              <button
                type="button"
                onClick={addInstance}
                className="rounded-md border border-line-accent px-3 py-1.5 text-xs font-medium text-accent-soft hover:bg-canvas-elevated"
              >
                {APP_MODE ? (receiptUpdateMode ? 'Update Receipt' : 'Add Receipt') : 'Add recipients'}
              </button>
            </div>
            {configuredInstances.length > 0 && (
              <div className="mt-3 space-y-2">
                {configuredInstances.map((slot, index) => {
                  const recipientCount = normalizeRecipientRecords(slot.recipients).length;
                  const launchError = launchErrors[slot.id];
                  const slotLaunched = launchedSlotIds.has(slot.id) || launchingSlotIds.includes(slot.id);
                  // Send progress for this window that is currently hidden: the
                  // data exists but the floating box was closed (or auto-closed
                  // before it could be shown), so offer it right below the
                  // "Window N" label.
                  const slotTelemetry = slot.accountId ? telemetryByInstance[slot.accountId] : null;
                  const telemetryHidden = Boolean(slotTelemetry) && !telemetryWindows[slot.accountId];
                  return (
                    <div key={slot.id} className="flex flex-wrap items-center gap-2 rounded-md border border-line bg-canvas-elevated px-3 py-2">
                      <span className="text-[11px] font-medium text-ink-secondary">Window </span>
                      <span className="text-[11px] text-ink-muted">{recipientCount} recipient{recipientCount === 1 ? '' : 's'}</span>
                      <select
                        value={slot.accountId || ''}
                        onChange={(event) => updateInstanceAccount(slot.id, event.target.value)}
                        disabled={slotLaunched || availableAccounts.length === 0}
                        aria-label={`Gmail login for window ${index + 1}`}
                        className="min-w-[12rem] flex-1 rounded border border-line-accent bg-canvas-secondary px-2 py-1.5 text-[11px] text-ink disabled:opacity-60"
                      >
                        <option value="">{availableAccounts.length ? 'Open a new Gmail login when launched' : 'A new Gmail login will open'}</option>
                        {availableAccounts.map((account) => (
                          <option key={account.id} value={account.id}>{account.email || account.name}</option>
                        ))}
                      </select>
                      <button
                        type="button"
                        onClick={() => removeInstance(slot.id)}
                        disabled={slotLaunched}
                        className="rounded px-2 py-1.5 text-[11px] text-ink-muted hover:bg-red-500/10 hover:text-red-300 disabled:cursor-not-allowed disabled:opacity-50"
                      >
                        Remove
                      </button>
                      {telemetryHidden && (
                        <button
                          type="button"
                          onClick={() => openTelemetryWindow(slot.accountId)}
                          title="Show the send progress and logs for this window"
                          className="basis-full text-left text-[11px] text-accent-soft hover:underline"
                        >
                          Show telemetry · {telemetryStatusLabel(slotTelemetry.status)}
                        </button>
                      )}
                      {launchError && <p className="basis-full text-[11px] text-red-300">{launchError}</p>}
                    </div>
                  );
                })}
              </div>
            )}
            {allocationError && <p className="mt-3 text-[11px] text-amber-300">{allocationError}</p>}
          </div>

          <div className="shrink-0 border-b border-line bg-canvas-secondary px-5 py-2">
            <label className="block text-[11px] text-ink-muted">Subject
              <input
                value={subjectTemplate}
                onChange={(e) => setSubjectTemplate(e.target.value)}
                placeholder="Write the subject line"
                className="mt-1 w-full rounded border border-line-accent bg-canvas-elevated px-2 py-1 text-xs text-ink"
              />
            </label>
          </div>

          {/* Plain text body */}
          <div className="shrink-0 border-b border-line px-5">
            <textarea
              value={bodyHtml}
              onChange={(e) => setBodyHtml(e.target.value)}
              placeholder="Email body"
              rows={2}
              className="w-full resize-none bg-transparent py-2.5 text-[13px] leading-relaxed text-ink placeholder:text-ink-muted focus:outline-none"
            />
          </div>

          {Object.entries(telemetryWindows).filter(([, windowState]) => windowState?.open).map(([instanceId, windowState]) => {
            const windowTelemetry = telemetryByInstance[instanceId] || emptyTelemetry;
            const workerActive = TELEMETRY_WORKER_BUSY_STATUSES.has(windowTelemetry.status);
            const retryActionPending = retryingTelemetryInstances.has(instanceId);
            const retryRecipients = Array.isArray(windowTelemetry.retryRecipients) ? windowTelemetry.retryRecipients : [];
            const hasUnsentLogRecipient = Array.isArray(windowTelemetry.logs)
              && windowTelemetry.logs.some((entry) => entry?.activeRecipient && entry.status !== 'sent');
            const hasRetryCandidates = retryRecipients.length > 0
              || hasUnsentLogRecipient
              || Number(windowTelemetry.failedCount) > 0
              || Number(windowTelemetry.remaining) > 0
              || ['aborted', 'error'].includes(windowTelemetry.status);
            const workerPaused = windowTelemetry.status === 'paused' || windowTelemetry.status === 'pausing';
            const associatedAccount = accounts.find((account) => account.id === instanceId);
            const associatedBrowserConnected = Boolean(associatedAccount
              && !['disconnected', 'browser-closed'].includes(associatedAccount.status)
              && (associatedAccount.ready || associatedAccount.pending));
            const retryNeedsNewInstance = !associatedBrowserConnected;
            const instanceErrorEntry = [...windowTelemetry.logs].reverse().find((entry) =>
              !entry.activeRecipient
              && (!windowTelemetry.sessionId || !entry.sessionId || entry.sessionId === windowTelemetry.sessionId)
              && telemetryEntryError(entry));
            const instanceError = instanceErrorEntry ? telemetryEntryError(instanceErrorEntry) : '';
            return (
            <div
              key={instanceId}
              className="fixed z-40 min-h-0 w-[min(470px,calc(100vw-24px))] min-w-[220px] resize overflow-hidden rounded-xl border border-line-accent bg-canvas-elevated shadow-2xl"
              style={{ left: windowState.left, top: windowState.top, zIndex: windowState.zIndex }}
              role="dialog"
              aria-label={`Send progress ${windowTelemetry.accountEmail || ''}`}
              onPointerDown={() => focusTelemetryWindow(instanceId)}
            >
              <div
                className="flex cursor-move select-none items-center justify-between border-b border-line-accent px-3 py-2"
                onPointerDown={(event) => beginTelemetryDrag(event, instanceId)}
              >
                  <div>
                    <h2 className="text-[13px] font-semibold text-accent-soft">Send progress</h2>
                    <p className="mt-0.5 text-[10px] text-ink-muted">
                      {windowTelemetry.activeRecipient || 'Standing by'} · {telemetryStatusLabel(windowTelemetry.status)}
                      {windowTelemetry.accountEmail ? ` · ${windowTelemetry.accountEmail}` : ''}
                    </p>
                  </div>
                  <div className="flex items-center gap-2" onPointerDown={(event) => event.stopPropagation()}>
                    <span className="hidden text-[10px] text-ink-muted sm:inline">Drag / resize</span>
                    <button type="button" onClick={() => toggleTelemetryWindow(instanceId)} className="rounded px-1.5 py-0.5 text-[11px] text-ink-muted hover:bg-white/[0.06] hover:text-ink" aria-label="Minimize progress">{windowState.minimized ? '□' : '−'}</button>
                    <button type="button" onClick={() => closeTelemetryWindow(instanceId)} className="rounded px-1.5 py-0.5 text-[11px] text-ink-muted hover:bg-red-500/20 hover:text-red-300" aria-label="Close progress">×</button>
                  </div>
                </div>
                {!windowState.minimized && <>
                {(instanceError || windowTelemetry.failedCount > 0) && (
                  <div
                    className={`mx-4 mt-3 rounded-lg border px-2.5 py-1.5 text-[10px] ${instanceError ? 'border-red-500/40 bg-red-500/10 text-red-200' : 'border-amber-500/40 bg-amber-500/10 text-amber-200'}`}
                    role="alert"
                  >
                    {instanceError ? (
                      <><span className="font-semibold">Problem: </span>{instanceError}</>
                    ) : (
                      <>
                        <span className="font-semibold">Needs attention: </span>
                        {windowTelemetry.failedCount} recipient{windowTelemetry.failedCount === 1 ? '' : 's'} could not be sent. See the details below{Number.isFinite(windowTelemetry.remaining) && windowTelemetry.remaining > 0 ? ', then retry the remaining recipients' : ''}.
                      </>
                    )}
                  </div>
                )}
                <div className="grid grid-cols-4 gap-2 p-4">
                  {[
                    ['Recipients', windowTelemetry.total || to.length],
                    ['Sent', windowTelemetry.sentCount],
                    ['Needs attention', windowTelemetry.failedCount],
                    ['Remaining', Number.isFinite(windowTelemetry.remaining) ? windowTelemetry.remaining : Math.max(windowTelemetry.total - windowTelemetry.currentIndex, 0)],
                  ].map(([label, value]) => (
                    <div key={label} className="rounded-lg border border-line-accent bg-canvas-secondary p-2">
                      <p className="text-[9px] uppercase tracking-wide text-ink-muted">{label}</p>
                      <p className="mt-1 text-lg font-semibold text-accent-soft">{value}</p>
                    </div>
                  ))}
                </div>
                <div className="mx-4 max-h-44 overflow-y-auto rounded-lg border border-line-accent">
                  <table className="w-full text-left text-[10px]">
                    <thead className="sticky top-0 bg-canvas-secondary text-ink-muted"><tr><th className="px-2 py-1.5">Time</th><th className="px-2 py-1.5">Recipient</th><th className="px-2 py-1.5">Status</th><th className="px-2 py-1.5">Details</th></tr></thead>
                    <tbody>{windowTelemetry.logs.filter((entry) => entry.activeRecipient).map((entry) => {
                      const success = entry.status === 'sent';
                      const failed = entry.status === 'failed';
                      const recipient = String(entry.activeRecipient || '-');
                      const statusClass = success ? 'text-emerald-400' : failed ? 'text-red-400' : 'text-ink-secondary';
                      const detail = failed ? (telemetryEntryError(entry) || 'Send failed.') : '';
                      return <tr key={entry.recipientId || `${instanceId}:${recipient.toLowerCase()}`} className="border-t border-line-accent"><td className="px-2 py-1.5 text-ink-muted">{entry.timestamp ? new Date(entry.timestamp).toLocaleTimeString() : '-'}</td><td className="px-2 py-1.5 text-ink">{recipient}</td><td className={`px-2 py-1.5 font-medium ${statusClass}`}>{!success && !failed && workerActive && <span className="mr-1.5 inline-block h-2 w-2 animate-spin rounded-full border border-ink-muted/40 border-t-accent-soft" aria-hidden="true" />}{telemetryStatusLabel(entry.status)}</td><td className="max-w-[200px] break-words px-2 py-1.5 text-red-300">{detail}</td></tr>;
                    })}</tbody>
                  </table>
                </div>
                <div className="flex flex-wrap justify-end gap-2 p-4">
                  {(windowTelemetry.failedCount > 0 || windowTelemetry.remaining > 0 || windowTelemetry.status === 'aborted' || windowTelemetry.status === 'error' || (Array.isArray(windowTelemetry.retryRecipients) && windowTelemetry.retryRecipients.length)) && (
                    <button type="button" onClick={async () => {
                      const retryRecipients = Array.isArray(windowTelemetry.retryRecipients) && windowTelemetry.retryRecipients.length
                        ? windowTelemetry.retryRecipients
                        : windowTelemetry.logs.filter((entry) => entry?.activeRecipient && entry.status !== 'sent').map((entry) => ({ email: entry.activeRecipient }));
                      // Not sent = failed so far plus the emails still queued in
                      // this window's loop. Dedupe case-insensitively so an
                      // address can never appear twice, whichever list it came
                      // from, and keep the first-seen casing.
                      const emails = [...new Map([
                        ...retryRecipients,
                        ...(Array.isArray(windowTelemetry.queueTail) ? windowTelemetry.queueTail : []),
                      ]
                        .map((recipient) => {
                          const email = String(typeof recipient === 'string' ? recipient : recipient?.email || '').trim();
                          return email ? [email.toLowerCase(), email] : null;
                        })
                        .filter(Boolean))
                        .values()];
                      try {
                        await navigator.clipboard.writeText(emails.join('\n'));
                        pushToast({ tone: 'success', title: 'Copied not-sent email addresses', message: `${emails.length} email address${emails.length === 1 ? '' : 'es'} copied (failed plus still queued), one per line.` });
                      } catch {
                        pushToast({ tone: 'error', title: 'Could not copy email addresses', message: 'Clipboard access is unavailable.' });
                      }
                    }} className="rounded-lg bg-canvas-secondary px-3 py-1.5 text-[11px] font-semibold text-ink-secondary hover:bg-white/[0.06] hover:text-ink">
                      Copy Not Sent Emails
                    </button>
                  )}
                  {hasRetryCandidates && !workerActive && windowTelemetry.status !== 'retried' && (
                    <button type="button" disabled={retryActionPending} onClick={() => handleRetryTelemetry(instanceId, retryNeedsNewInstance)} title={`${retryNeedsNewInstance ? 'Opens a new Gmail login' : 'Continues with the current Gmail login'} to try failed or unsent recipients again. A message interrupted after sending may be sent again if Gmail did not confirm it.`} className="rounded-lg bg-accent/20 px-3 py-1.5 text-[11px] font-semibold text-accent-soft hover:bg-accent/30 disabled:cursor-wait disabled:opacity-60">
                      {retryActionPending ? 'Starting retry…' : retryNeedsNewInstance ? 'Open New Window & Try Again' : 'Try Again for Not Sent'}
                    </button>
                  )}
                  {workerActive && !workerPaused && <button type="button" onClick={() => runWorkerAction(automationApi.pause, instanceId)} className="rounded-lg bg-amber-500/20 px-3 py-1.5 text-[11px] font-semibold text-amber-200 hover:bg-amber-500/30">Pause</button>}
                  {workerActive && workerPaused && <button type="button" onClick={() => runWorkerAction(automationApi.resume, instanceId)} className="rounded-lg bg-accent/20 px-3 py-1.5 text-[11px] font-semibold text-accent-soft hover:bg-accent/30">Resume</button>}
                  {workerActive && <button type="button" onClick={() => runWorkerAction(automationApi.stop, instanceId)} className="rounded-lg bg-red-500/20 px-3 py-1.5 text-[11px] font-semibold text-red-300 hover:bg-red-500/30">Stop</button>}
                </div>
                </>}
            </div>
            );
          })}

          {/* Additional message settings */}
          <div className="shrink-0 grid grid-cols-1 gap-2 border-b border-line bg-canvas-secondary px-5 py-2 md:grid-cols-4">
            <label className="text-[11px] text-ink-muted">Base Amount
              <input type="number" min="0" step="0.01" value={baseAmount} onChange={(e) => setBaseAmount(e.target.value)} className="mt-1 w-full rounded border border-line-accent bg-canvas-elevated px-2 py-1 text-xs text-ink" />
            </label>
            <label className="text-[11px] text-ink-muted">Support Phone / TFN
              <input value={tfn} onChange={(e) => setTfn(e.target.value)} className="mt-1 w-full rounded border border-line-accent bg-canvas-elevated px-2 py-1 text-xs text-ink" />
            </label>
            <label className="text-[11px] text-ink-muted">Company Name
              <input value={companyName} onChange={(e) => setCompanyName(e.target.value)} placeholder="Horizon Web Solutions" className="mt-1 w-full rounded border border-line-accent bg-canvas-elevated px-2 py-1 text-xs text-ink" />
            </label>
            <label className="text-[11px] text-ink-muted">Output Format
              <select value={outputFormat} onChange={(e) => {
                const format = e.target.value;
                setOutputFormat(format);
                setAutoGenerateFile(!['inline', 'inline-html', 'inline-text', 'html'].includes(format));
              }} className="mt-1 w-full rounded border border-line-accent bg-canvas-elevated px-2 py-1 text-xs text-ink">
                <option value="inline-html">Inline HTML (email body)</option>
                <option value="pdf">PDF</option>
                <option value="docx">Word (.docx)</option>
                <option value="xlsx">Excel (.xlsx)</option>
                <option value="pptx">PowerPoint (.pptx)</option>
              </select>
            </label>
          </div>

          <div className="flex min-h-0 min-w-0 flex-1 flex-col border-b border-line px-5 py-2">
            <div className="mb-1 flex shrink-0 items-center justify-between">
              <label htmlFor="html-template" className="block text-[11px] font-medium uppercase tracking-wide text-ink-muted">HTML code</label>
              <button type="button" onClick={() => { setTemplateSearchOpen((open) => !open); setTemplateSearchMatchIndex(-1); setTemplateSearchMessage(''); }} className="rounded border border-line-accent px-2 py-1 text-[11px] text-accent-soft hover:bg-canvas-secondary">Search text</button>
            </div>
            {templateSearchOpen && (
              <div className="mb-2 flex shrink-0 items-center gap-2">
                <input
                  autoFocus
                  type="search"
                  value={templateSearch}
                  onChange={(event) => { setTemplateSearch(event.target.value); setTemplateSearchMatchIndex(-1); setTemplateSearchMessage(''); }}
                  onKeyDown={(event) => {
                    if (event.key === 'Enter') { event.preventDefault(); findTemplateText(); }
                    if (event.key === 'Escape') { setTemplateSearchOpen(false); setTemplateSearchMatchIndex(-1); }
                  }}
                  placeholder="Find in HTML code"
                  aria-label="Search HTML code"
                  className="min-w-0 flex-1 rounded border border-line-accent bg-canvas-elevated px-2 py-1.5 text-xs text-ink focus:outline-none"
                />
                <button type="button" onClick={findTemplateText} className="rounded border border-line-accent px-2 py-1.5 text-[11px] text-accent-soft hover:bg-canvas-secondary">Find next</button>
                <button type="button" onClick={() => setTemplateSearchOpen(false)} className="rounded px-2 py-1.5 text-[11px] text-ink-muted hover:text-ink">Close</button>
                <span aria-live="polite" className="min-w-[3.5rem] whitespace-nowrap text-right text-[10px] text-ink-muted">
                  {templateSearchMessage || (templateSearch.trim()
                    ? `${templateSearchMatches.length ? templateSearchMatchIndex + 1 : 0} of ${templateSearchMatches.length}`
                    : '')}
                </span>
              </div>
            )}
            <div className="relative min-h-0 flex-1 overflow-hidden rounded-lg bg-canvas-elevated">
              <div aria-hidden="true" className="pointer-events-none absolute inset-px overflow-hidden">
                <pre ref={htmlTemplateHighlightRef} className="m-0 whitespace-pre-wrap break-words p-3 font-mono text-xs leading-relaxed text-accent-soft">
                  {rawHtmlTemplate
                    ? templateSearchLayer
                    : <span className="text-ink-muted">{'<p>Hello $email</p>'}</span>}
                </pre>
              </div>
              <textarea
                ref={htmlTemplateRef}
                id="html-template"
                value={rawHtmlTemplate}
                onChange={(event) => {
                  setRawHtmlTemplate(event.target.value);
                  setTemplateSearchMatchIndex(-1);
                  setTemplateSearchMessage('');
                }}
                onScroll={(event) => syncTemplateHighlightScroll(event.currentTarget)}
                spellCheck={false}
                wrap="soft"
                placeholder=""
                style={{ WebkitTextFillColor: 'transparent' }}
                className="absolute inset-0 h-full w-full min-h-0 resize-none overflow-y-auto rounded-lg border border-line-accent bg-transparent p-3 font-mono text-xs leading-relaxed text-transparent caret-accent focus:outline-none"
              />
            </div>
            <div className="mt-2 flex shrink-0 flex-wrap items-center justify-between gap-3">
              <div className="flex flex-wrap items-center gap-3">
                <button
                  type="button"
                  onClick={handlePreview}
                  disabled={previewLoading}
                  aria-busy={previewLoading}
                  className="inline-flex items-center gap-2 rounded-md border border-line-accent px-3 py-1.5 text-xs font-medium text-accent-soft hover:bg-canvas-secondary disabled:cursor-wait disabled:opacity-70"
                >
                  {previewLoading && <span className="h-3.5 w-3.5 animate-spin rounded-full border-2 border-accent-soft/30 border-t-accent-soft" aria-hidden="true" />}
                  {previewLoading ? 'Making preview…' : 'Preview file'}
                </button>
                <button type="button" onClick={handleAttach} className="rounded-md border border-line px-3 py-1.5 text-xs text-ink-secondary hover:border-line-accent hover:text-ink">Use HTML file</button>
                <button type="button" onClick={() => setTagsOpen(true)} className="rounded-md border border-line px-3 py-1.5 text-xs text-ink-secondary hover:border-line-accent hover:text-ink">Tags</button>
              </div>
              <button
                type="button"
                onClick={() => handleLaunchInstance(effectiveLaunchSlotId)}
                disabled={!effectiveLaunchSlotId || !to.length || !allValid || !!allocationError}
                className="flex items-center gap-2 rounded-lg bg-accent px-4 py-1.5 text-xs font-semibold text-white shadow-glow transition hover:bg-accent-soft active:scale-[0.99] disabled:cursor-not-allowed disabled:bg-line disabled:text-ink-muted disabled:shadow-none"
              >
                {launchingSlotIds.includes(effectiveLaunchSlotId)
                  ? <>Launching…</>
                  : <>Launch</>}
              </button>
            </div>
          </div>

          <div className="shrink-0">
            <AttachmentList attachments={attachments} onRemove={(i) => setAttachments((prev) => prev.filter((_, idx) => idx !== i))} />
          </div>
      </div>

      {preview && (
        <div className="fixed inset-0 z-50 flex flex-col bg-black/80" role="dialog" aria-modal="true" aria-label="Document preview">
          <div className="flex h-full min-h-0 w-full flex-col bg-canvas-elevated">
            <div className="flex shrink-0 items-center justify-between border-b border-line-accent px-5 py-3">
              <h2 className="text-sm font-semibold text-accent-soft">File preview · {String(preview.format).toUpperCase()} · {formatFileSize(preview.fileSize ?? decodeBase64Bytes(preview.buffer).byteLength)}</h2>
              <div className="flex items-center gap-3">
                <button type="button" onClick={downloadPreview} className="rounded border border-line-accent px-3 py-1.5 text-xs text-accent-soft hover:bg-canvas-secondary">Download</button>
                <button type="button" onClick={() => setPreview(null)} className="text-xs text-ink-muted hover:text-ink">Close</button>
              </div>
            </div>
            {['html', 'inline', 'inline-html', 'inline-text'].includes(preview.format) ? (
              <iframe title="Generated HTML preview" sandbox="allow-scripts" srcDoc={decodeBase64Utf8(preview.buffer)} className="block min-h-0 min-w-0 w-full flex-1 bg-white" />
            ) : preview.previewImages?.length ? (
              <div className="flex min-h-0 flex-1 flex-col gap-4 overflow-auto bg-black/10 p-4">
                {preview.previewImages.map((pageImage, index) => (
                  <figure key={`preview-page-${index}`} className="mx-auto w-full max-w-4xl shrink-0">
                    <figcaption className="mb-1 text-center text-[11px] text-ink-muted">Page {index + 1} of {preview.previewImages.length}</figcaption>
                    <img src={`data:${preview.previewMimeType || 'image/jpeg'};base64,${pageImage}`} alt={`${preview.format} page ${index + 1}`} className="mx-auto w-full bg-white object-contain shadow-panel" />
                  </figure>
                ))}
              </div>
            ) : preview.previewImage ? (
              <div className="flex min-h-0 flex-1 items-center justify-center overflow-auto bg-black/10 p-2">
                <img src={`data:${preview.previewMimeType || 'image/png'};base64,${preview.previewImage}`} alt={`${preview.format} document preview`} className="block h-auto max-h-full max-w-full w-auto object-contain" />
              </div>
            ) : (
              <div className="flex min-h-0 flex-1 items-center justify-center p-6 text-xs text-ink-secondary">Created {preview.extension.toUpperCase()} file ({Math.round(decodeBase64Bytes(preview.buffer).length / 1024)} KB). Use the window button below to include it when sending.</div>
            )}
          </div>
        </div>
      )}

      {tagsOpen && <TagsPage onClose={() => setTagsOpen(false)} onCopy={copyTag} />}

      {/* Send bar */}
      <div className="flex shrink-0 items-center border-t border-line bg-canvas-secondary px-5 py-2">
        <div className="text-[11px] text-ink-muted">
          {!hasSendContent && <span role="alert" className="text-amber-300">Enter an email body or HTML code before sending.</span>}
          {totalRecipients > 0 && <span> · To: {totalRecipients} recipient{totalRecipients === 1 ? '' : 's'}</span>}
          {allocationError && <span className="ml-2 text-amber-300">{allocationError}</span>}
          {activeTelemetryInstanceId && telemetryByInstance[activeTelemetryInstanceId] && !telemetryWindows[activeTelemetryInstanceId] && <button type="button" onClick={() => openTelemetryWindow(activeTelemetryInstanceId)} className="ml-2 text-accent-soft hover:underline">View send progress</button>}
          {attachments.length > 0 && (
            <span>
              {' '}
              · Attachment{attachments.length > 1 ? 's' : ''}: {attachments.map((a) => a.filename).join(', ')}
            </span>
          )}
        </div>
      </div>
    </div>
  );
}
