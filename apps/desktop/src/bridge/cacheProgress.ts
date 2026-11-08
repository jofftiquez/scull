import type { DesktopCacheProgress } from './generated';
import { newOpId, realTransport, type TauriTransport } from './tauri';
import { toBridgeError } from './types';

export const CACHE_PROGRESS_EVENT = 'scull://cache-clean-progress';
export type CacheProgressHandler = (event: DesktopCacheProgress) => void;

/** Listen before starting; only this invocation's events reach its task. */
export async function runCacheCommand<T>(command: string, args: Record<string, unknown>, onProgress?: CacheProgressHandler, transport: TauriTransport = realTransport): Promise<T> {
  let stop: (() => void) | undefined;
  try {
    if (onProgress) {
      const opId = newOpId();
      stop = await transport.listen<DesktopCacheProgress>(CACHE_PROGRESS_EVENT, (event) => { if (event.op_id === opId) onProgress(event); });
      args = { ...args, opId };
    }
    return await transport.invoke<T>(command, args);
  } catch (error) { throw toBridgeError(error); }
  finally { stop?.(); }
}
