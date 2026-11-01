import React from 'react';

export default function LoadingScreen({ label = 'Loading' }) {
  return (
    <div className="flex flex-1 flex-col items-center justify-center gap-4 bg-canvas text-ink-secondary">
      <div className="relative h-8 w-8">
        <div className="absolute inset-0 rounded-full border-2 border-line" />
        <div className="absolute inset-0 animate-spin rounded-full border-2 border-transparent border-t-accent-soft" />
      </div>
      <p className="text-xs tracking-wide text-ink-muted">{label}</p>
    </div>
  );
}
