// Entry of the main window (index.html).
import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import '@fontsource/dm-sans/400.css';
import '@fontsource/dm-sans/500.css';
import '@fontsource/dm-sans/600.css';
import '@fontsource/dm-mono/400.css';
import './styles/main.css';
import { selectBridge } from './bridge';
import { planBridgeFor } from './bridge/plan';
import { App } from './App';
import { BridgeProvider } from './bridgeContext';

const core = selectBridge();

createRoot(document.getElementById('app')!).render(
  <StrictMode>
    <BridgeProvider value={{ core, plans: planBridgeFor(core) }}>
      <App />
    </BridgeProvider>
  </StrictMode>,
);
