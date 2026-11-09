// Picks the bridge: Tauri's when the page runs inside the app, the mock otherwise.
import { createMockBridge } from './mock';
import { createTauriBridge } from './tauri';
import type { ScullBridge } from './types';

export * from './types';

/** Whether the page runs inside Tauri (`withGlobalTauri` / the IPC object is present). */
export function insideTauri(w: unknown = globalThis): boolean {
  const g = w as { __TAURI__?: unknown; __TAURI_INTERNALS__?: unknown };
  return g.__TAURI__ !== undefined || g.__TAURI_INTERNALS__ !== undefined;
}

export function selectBridge(w: unknown = globalThis): ScullBridge {
  return insideTauri(w)
    ? createTauriBridge()
    : createMockBridge({ latencyMs: 120, progressMs: 250 });
}
