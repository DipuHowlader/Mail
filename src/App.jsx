import React from 'react';
import TitleBar from './components/TitleBar.jsx';
import ToastStack from './components/Toast.jsx';
import ComposerPage from './pages/ComposerPage.jsx';
import LoadingScreen from './components/LoadingScreen.jsx';
import { useAppContext } from './context/AppContext.jsx';
import { runtimeInfo } from './services/ipc.js';

export default function App() {
  const { stage, theme } = useAppContext();
  const appName = runtimeInfo.instanceName || 'Novamail';
  const label = `Compose Email`;

  return (
    <div data-theme={theme} className="flex h-screen w-screen flex-col overflow-hidden bg-canvas text-ink">
      <TitleBar label={label} />
      <div className="flex min-h-0 min-w-0 flex-1 overflow-hidden">
        {stage === 'composer' ? <ComposerPage /> : <LoadingScreen label="Starting…" />}
      </div>
      <ToastStack />
    </div>
  );
}
