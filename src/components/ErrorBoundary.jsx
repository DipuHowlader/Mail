import React from 'react';

export default class ErrorBoundary extends React.Component {
  constructor(props) {
    super(props);
    this.state = { error: null };
  }

  static getDerivedStateFromError(error) {
    return { error };
  }

  componentDidCatch(error, info) {
    // Kept for troubleshooting only. The user never sees this text.
    console.error('[novamail] Renderer error:', error, info?.componentStack || '');
  }

  render() {
    if (this.state.error) {
      return (
        <div className="flex h-screen w-screen flex-col items-center justify-center gap-4 bg-canvas p-8 text-center text-ink">
          <h1 className="text-lg font-semibold">Something went wrong</h1>
          <p className="max-w-md text-sm text-ink-secondary">
            The app ran into an unexpected problem. Reloading usually fixes it.
            If it keeps happening, close the app and open it again.
          </p>
          <button
            type="button"
            className="rounded bg-accent px-4 py-2 text-sm font-medium text-white"
            onClick={() => window.location.reload()}
          >
            Reload app
          </button>
        </div>
      );
    }
    return this.props.children;
  }
}
