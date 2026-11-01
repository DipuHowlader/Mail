// Turns internal errors into plain language a normal user can act on.
//
// Every message the app shows comes from one of two places: a friendly string the
// caller already wrote, or a raw error raised deep in the desktop process (Gmail
// automation, document rendering, file access). The raw errors are written for
// developers — they mention selectors, Chromium, CDP, file paths and stack traces —
// so they must never be shown as-is. `translateToFriendly` maps the common ones to a
// short explanation plus a next step, and `userFacingError` is the entry point used
// by toast and inline error call sites.

const FRIENDLY_RULES = [
  // ------------------------------------------------------------------ Packaging
  // The bundled Chromium/engine is missing or could not start.
  [
    /\b(?:chromium|clearcote|chrome\.exe|executablePath|browser executable|browser engine|bundled browser)\b/i,
    'A required part of NovaMail is missing. Please reinstall the app and try again.',
  ],
  // ------------------------------------------------------------------ Gmail session
  [
    /\b(?:gmail session|sign(?:ed)? in|signed-in|sign-in|login is required|waiting-for-login|account could not be identified|retry_account_mismatch|google_automation_signin_blocked|no active gmail tab)\b/i,
    'Gmail is not signed in. Open the Gmail window, sign in, then try again.',
  ],
  // ------------------------------------------------------------------ Compose: recipient
  [
    /\b(?:recipient address|commit the recipient|to field|recipient chip|add the recipient)\b/i,
    'Gmail did not accept the recipient address. Check the email address, then try again.',
  ],
  // ------------------------------------------------------------------ Compose: subject
  [
    /\b(?:subject value|subject field|subject template)\b/i,
    'Gmail did not accept the subject. Please try again.',
  ],
  // ------------------------------------------------------------------ Compose: body
  [
    /\b(?:compose body|email body|insertable email content|html body|inline html)\b/i,
    'Gmail did not accept the message body. Please try again.',
  ],
  // ------------------------------------------------------------------ Compose: window / send
  [
    /\b(?:send button|send the message|message sent|did not send|could not send|sending the message)\b/i,
    'The message could not be sent through Gmail. Please check the Gmail window, then try again.',
  ],
  [
    /\b(?:compose button|compose window|could not be opened|compose was not opened)\b/i,
    'The Gmail message window could not be opened. Make sure Gmail is open and signed in, then try again.',
  ],
  // ------------------------------------------------------------------ Attachments / files
  [
    /\b(?:25 ?mb|too large|exceeds|maximum|100 ?kb|file size)\b/i,
    'A file is larger than Gmail allows. Please use a smaller file.',
  ],
  [
    /\b(?:attachment|staged file)\b.*\b(?:missing|not readable|empty|invalid|failed|not found)\b|\b(?:missing or empty|not readable)\b/i,
    'An attachment could not be read. Please add the file again, then try again.',
  ],
  // ------------------------------------------------------------------ Document generation
  [
    /\b(?:render template|document|pdf|xlsx|docx|pptx|generate|rasteriz|canvas)\b/i,
    'The attached document could not be created. Please check the template, then try again.',
  ],
  // ------------------------------------------------------------------ Recipients / campaign setup
  [
    /\b(?:valid email|recipient)\b/i,
    'Please check the recipient email addresses, then try again.',
  ],
  [
    /\b(?:template|variable|\{\{)\b/i,
    'There is a problem with the template. Please check it, then try again.',
  ],
  [
    /\b(?:browser instance|automation assignment|allocation|launch)\b/i,
    'A browser window could not be started. Close any extra windows, then try again.',
  ],
  // ------------------------------------------------------------------ Networking
  [
    /\b(?:fetch|network|offline|dns|econn|etimedout|timeout|abort)\b/i,
    'NovaMail could not reach the network. Check your internet connection, then try again.',
  ],
];

// Wrappers desktop runtime adds around errors that cross the IPC boundary, plus a plain
// "Error:" prefix. Stripped before matching so the rules see the real message.
function stripErrorNoise(message) {
  return String(message || '')
    .replace(/^Error invoking remote method '[^']*':\s*/i, '')
    .replace(/^(?:Uncaught\s+)?(?:Error|TypeError|RangeError|ReferenceError):\s*/i, '')
    .replace(/\s+/g, ' ')
    .trim();
}

// Matches messages that are clearly written for developers even when no rule
// above recognizes them, so they are replaced by the friendly fallback.
const TECHNICAL_ERROR = /\b(?:UND_ERR_[A-Z_]+|ERR_[A-Z_]+|ECONN[A-Z_]+|ETIMEDOUT|AbortError|TimeoutError)\b|Error invoking remote method|Cannot find module|Require stack|node_modules|stack trace|\b(?:[A-Z]:\\|\\\\)|Cannot read properties of|is not a function|is not iterable|Mismatched|Unclosed|Unexpected (?:end|token|character|trailing)|\bIPC\b|\bCDP\b|selector|querySelector|getBoundingClientRect|\bDOM\b|innerText|textContent|aria-|\[role=|napi|\.node\b/i;

/**
 * Map one raw error message to plain language. Returns the original string when it
 * already reads like a sentence written for people.
 */
export function translateToFriendly(rawMessage) {
  const message = stripErrorNoise(rawMessage);
  if (!message) return '';
  for (const [pattern, friendly] of FRIENDLY_RULES) {
    if (pattern.test(message)) return friendly;
  }
  return message;
}

export function userFacingError(error, fallback) {
  const rawMessage = typeof error === 'string' ? error : error?.message;
  const message = stripErrorNoise(rawMessage);
  if (!message) return fallback;
  const friendly = translateToFriendly(message);
  if (friendly && friendly !== message) return friendly;
  // No rule matched. Keep a short, human-sounding message; otherwise fall back.
  if (message.length > 240 || TECHNICAL_ERROR.test(message)) return fallback;
  return message;
}

// Extracts the full, unsanitized details of an error. Kept for diagnostics and
// support tooling; it is intentionally NOT shown in toasts anymore.
export function describeError(error) {
  if (error === undefined || error === null) return null;
  if (typeof error === 'string') {
    const message = error.replace(/\s+/g, ' ').trim();
    return message ? { message } : null;
  }
  const message = String(error?.message ?? '').replace(/\s+/g, ' ').trim();
  const code = error?.code != null && String(error.code).trim() ? String(error.code).trim() : undefined;
  const stack = typeof error?.stack === 'string' && error.stack.trim() ? error.stack.trim() : undefined;
  if (!message && !code && !stack) return null;
  return { message: message || undefined, code, stack };
}
