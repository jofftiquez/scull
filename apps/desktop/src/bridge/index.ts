// Picks the bridge: Tauri's when the page runs inside the app, the mock otherwise.
import type { InjectionKey } from 'vue';
import { inject } from 'vue';
import { createMockBridge } from './mock';
import { createTauriBridge } from './tauri';
import type { ScullBridge } from './types';

export * from './types';

/** Whether the page runs inside Tauri (`withGlobalTauri` / the IPC object is present). */
export function insideTauri(w: unknown = globalThis): boolean {
  const g = w as { __TAURI__?: unknown; __TAURI_INTERNALS__?: unknown };
  return g.__TAURI__ !== undefined || g.__TAURI_INTERNALS__ !== undefined;
}

/** Whether the page asked the mock for a never-scanned machine (`?scenario=first-run`, E2E). */
export function wantsFirstRun(w: unknown = globalThis): boolean {
  const loc = (w as { location?: { search?: string; hash?: string } } | undefined)?.location;
  if (!loc) return false;
  const q = `${loc.search ?? ''}&${(loc.hash ?? '').split('?')[1] ?? ''}`;
  return /(^|[?&])scenario=first-run(&|$)/.test(q);
}

export function selectBridge(w: unknown = globalThis): ScullBridge {
  return insideTauri(w)
    ? createTauriBridge()
    : createMockBridge({ latencyMs: 120, progressMs: 250, firstRun: wantsFirstRun(w) });
}

export const BRIDGE_KEY: InjectionKey<ScullBridge> = Symbol('scull-bridge');

/** The bridge provided by the app root. */
export function useBridge(): ScullBridge {
  const b = inject(BRIDGE_KEY);
  if (!b) throw new Error('no ScullBridge provided');
  return b;
}
