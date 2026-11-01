// Light / dark theme (P5-UI-01). The palette lives in design/tokens.json and the generated
// src/styles/theme.css: by default it follows the system (`prefers-color-scheme`), and the
// Settings > General > Appearance choice can force one by setting <html data-theme>. The choice
// is a display preference of this machine, not scull config, so it is kept in localStorage
// (shared by the main window and the menu bar widget, which follow each other through the
// `storage` event) and never goes through the settings writer.
import { ref, type Ref } from 'vue';

export type ThemePref = 'system' | 'light' | 'dark';

export const THEME_KEY = 'scull.theme';

/** Segmented options for Settings: `[value, label]`. */
export const THEME_OPTIONS: readonly (readonly [ThemePref, string])[] = [
  ['system', 'System'],
  ['light', 'Light'],
  ['dark', 'Dark'],
];

export function isThemePref(v: unknown): v is ThemePref {
  return v === 'system' || v === 'light' || v === 'dark';
}

/** localStorage, or null where it is missing or blocked (storage access can throw). */
function storage(): Storage | null {
  try {
    return globalThis.localStorage ?? null;
  } catch {
    return null;
  }
}

/** The saved preference; `system` when nothing (or something unknown) is saved. */
export function readThemePref(store: Storage | null = storage()): ThemePref {
  try {
    const v = store?.getItem(THEME_KEY);
    return isThemePref(v) ? v : 'system';
  } catch {
    return 'system';
  }
}

/** Apply a preference to the document: `system` removes the override. */
export function applyTheme(pref: ThemePref, root: HTMLElement | undefined = globalThis.document?.documentElement): void {
  if (!root) return;
  if (pref === 'system') delete root.dataset.theme;
  else root.dataset.theme = pref;
}

/** The theme in effect: the forced one, else the system's. */
export function resolvedTheme(pref: ThemePref, media: (q: string) => boolean = systemMatches): 'light' | 'dark' {
  if (pref !== 'system') return pref;
  return media('(prefers-color-scheme: dark)') ? 'dark' : 'light';
}

function systemMatches(q: string): boolean {
  return typeof globalThis.matchMedia === 'function' && globalThis.matchMedia(q).matches;
}

const current = ref<ThemePref>(readThemePref());
let listening = false;

/**
 * Apply the saved preference before the app mounts (no flash of the wrong theme) and follow
 * changes made in another window of the app (the menu bar widget and the main window).
 */
export function initTheme(): void {
  current.value = readThemePref();
  applyTheme(current.value);
  if (listening || typeof globalThis.addEventListener !== 'function') return;
  listening = true;
  globalThis.addEventListener('storage', (e: StorageEvent) => {
    if (e.key !== null && e.key !== THEME_KEY) return;
    current.value = readThemePref();
    applyTheme(current.value);
  });
}

/** The preference (reactive) and a setter that saves and applies it. */
export function useTheme(): { pref: Readonly<Ref<ThemePref>>; setTheme: (p: ThemePref) => void } {
  return {
    pref: current,
    setTheme(p: ThemePref) {
      current.value = p;
      try {
        const store = storage();
        if (p === 'system') store?.removeItem(THEME_KEY);
        else store?.setItem(THEME_KEY, p);
      } catch {
        // Not saved (storage blocked): the choice still applies until the window closes.
      }
      applyTheme(p);
    },
  };
}
