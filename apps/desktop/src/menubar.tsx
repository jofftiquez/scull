// Entry of the menu bar / tray widget (menubar.html): one button that opens the main window.
// The tray itself is trimmed in the next step of the desktop restart (#144).
import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import '@fontsource/dm-sans/400.css';
import '@fontsource/dm-sans/500.css';
import './styles/main.css';
import { insideTauri } from './bridge';
import { createTauriWindowCommands } from './bridge/tauri';
import { Button } from './ui';

function MenuBar() {
  const open = () => {
    if (insideTauri()) void createTauriWindowCommands().openMain('/');
  };
  return (
    <div className="flex items-center justify-between gap-3 p-4">
      <span className="text-h3 font-semibold">scull</span>
      <Button variant="primary" onClick={open}>
        Open scull
      </Button>
    </div>
  );
}

createRoot(document.getElementById('app')!).render(
  <StrictMode>
    <MenuBar />
  </StrictMode>,
);
