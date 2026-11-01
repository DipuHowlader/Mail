import React from 'react';
import { formatBytes } from '../utils/validation.js';

function FileIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 14 14" fill="none">
      <path d="M4 1.5h4l3 3V11a1.5 1.5 0 01-1.5 1.5h-6A1.5 1.5 0 012 11V3A1.5 1.5 0 014 1.5z" stroke="#60a5fa" strokeWidth="1.2" strokeLinejoin="round" />
      <path d="M8 1.5V5h3" stroke="#60a5fa" strokeWidth="1.2" strokeLinejoin="round" />
    </svg>
  );
}

export default function AttachmentList({ attachments, onRemove }) {
  if (!attachments.length) return null;

  return (
    <div className="flex flex-wrap gap-2 border-t border-line px-5 py-3">
      {attachments.map((file, i) => (
        <div
          key={`${file.filename}-${i}`}
          className={`flex items-center gap-2.5 rounded-lg border px-3 py-2 ${
            file.tooLarge ? 'border-red-500/40 bg-red-950/30' : 'border-line bg-canvas-secondary'
          }`}
        >
          <span className="flex h-7 w-7 shrink-0 items-center justify-center rounded-md bg-accent/10">
            <FileIcon />
          </span>
          <div className="min-w-0">
            <p className="max-w-[160px] truncate text-xs font-medium text-ink">{file.filename}</p>
            <p className={`text-[11px] ${file.tooLarge ? 'text-red-400' : 'text-ink-muted'}`}>
              {file.tooLarge ? 'Too large — 25 MB max' : formatBytes(file.size)}
            </p>
          </div>
          <button
            type="button"
            onClick={() => onRemove(i)}
            aria-label={`Remove ${file.filename}`}
            className="ml-1 shrink-0 rounded p-1 text-ink-muted transition hover:bg-white/10 hover:text-ink"
          >
            <svg width="10" height="10" viewBox="0 0 10 10"><path d="M1 1l8 8M9 1l-8 8" stroke="currentColor" strokeWidth="1.3" /></svg>
          </button>
        </div>
      ))}
    </div>
  );
}
