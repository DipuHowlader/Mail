// Install the native bridge (Tauri) before anything reads window.api.
import './services/tauriBridge.js';
import React from 'react';
import ReactDOM from 'react-dom/client';
import App from './App.jsx';
import LauncherApp from './launcher/LauncherApp.jsx';
import LicenseGate from './components/LicenseGate.jsx';
import TitleBar from './components/TitleBar.jsx';
import { AppProvider } from './context/AppContext.jsx';
import ErrorBoundary from './components/ErrorBoundary.jsx';
import { runtimeInfo } from './services/ipc.js';
import './index.css';

// One bundle, two roles:
//   - the launcher window shows the app manager,
//   - an app window shows a single, fully independent app.
//
// Both windows are frameless, so each role draws its own title bar (see
// `src/components/TitleBar.jsx`). The launcher needs it above the license gate
// too, otherwise the window cannot be moved, minimized or closed at all.
const tree = runtimeInfo.isLauncher
  ? (
    <div className="flex h-screen w-screen flex-col overflow-hidden bg-canvas text-ink">
      <TitleBar label="Dashboard" />
      <div className="flex min-h-0 min-w-0 flex-1 flex-col overflow-hidden">
        <LicenseGate>
          <LauncherApp />
        </LicenseGate>
      </div>
    </div>
  )
  : (
    <AppProvider>
      <App />
    </AppProvider>
  );

ReactDOM.createRoot(document.getElementById('root')).render(
  <React.StrictMode>
    <ErrorBoundary>
      {tree}
    </ErrorBoundary>
  </React.StrictMode>
);
