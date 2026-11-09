// The app shell: a header and two views, Clean (the cleanup flow) and Caches.
import { useState } from 'react';
import { Caches } from './Caches';
import { CleanFlow } from './CleanFlow';

type View = 'clean' | 'caches';

const VIEWS: { id: View; label: string }[] = [
  { id: 'clean', label: 'Clean' },
  { id: 'caches', label: 'Caches' },
];

export function App() {
  const [view, setView] = useState<View>('clean');
  return (
    <div className="mx-auto flex min-h-screen max-w-5xl flex-col gap-6 px-6 py-6">
      <header className="flex items-center justify-between gap-4">
        <h1 className="m-0 text-h2 font-semibold tracking-tight">scull</h1>
        <nav role="tablist" aria-label="Views" className="flex gap-1 rounded-button bg-surface-raised p-1">
          {VIEWS.map((v) => (
            <button
              key={v.id}
              type="button"
              role="tab"
              id={`tab-${v.id}`}
              aria-selected={view === v.id}
              aria-controls={`panel-${v.id}`}
              onClick={() => setView(v.id)}
              className={`rounded-button px-4 py-1.5 text-label font-medium ${
                view === v.id ? 'bg-surface text-ink' : 'text-text-secondary hover:text-ink'
              }`}
            >
              {v.label}
            </button>
          ))}
        </nav>
      </header>
      <main id={`panel-${view}`} role="tabpanel" aria-labelledby={`tab-${view}`}>
        {view === 'clean' ? <CleanFlow /> : <Caches />}
      </main>
    </div>
  );
}
