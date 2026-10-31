// The real bridge: Tauri commands implemented in apps/desktop/src-tauri (thin wrappers over
// scull-desktop-bridge, which calls the read-only scull crates). The only module allowed to
// import @tauri-apps/api (enforced by eslint `no-restricted-imports`).
import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import {
  CANCEL_COMMAND,
  COMMANDS,
  PROGRESS_EVENT,
  toBridgeError,
  type OperationOptions,
  type ProgressEvent,
  type ScullBridge,
} from './types';

/** The transport, injectable for tests. */
export interface TauriTransport {
  invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T>;
  listen<T>(event: string, handler: (payload: T) => void): Promise<UnlistenFn>;
}

const realTransport: TauriTransport = {
  invoke: (cmd, args) => invoke(cmd, args),
  listen: (event, handler) => listen(event, (e) => handler(e.payload as never)),
};

/** A fresh operation ID (matches the core's `[A-Za-z0-9_-]{1,64}`). */
export function newOpId(): string {
  const c = globalThis.crypto;
  if (c && 'randomUUID' in c) return c.randomUUID();
  return `op-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}

export function createTauriBridge(t: TauriTransport = realTransport): ScullBridge {
  const call = async <T>(cmd: string, args?: Record<string, unknown>): Promise<T> => {
    try {
      return await t.invoke<T>(cmd, args);
    } catch (e) {
      throw toBridgeError(e);
    }
  };

  /** Run a long operation: progress events filtered by its ID, abort -> cancel_operation. */
  const operation = async <T>(cmd: string, args: Record<string, unknown>, opts: OperationOptions = {}): Promise<T> => {
    const opId = newOpId();
    let unlisten: UnlistenFn | undefined;
    if (opts.onProgress) {
      const onProgress = opts.onProgress;
      unlisten = await t.listen<ProgressEvent>(PROGRESS_EVENT, (p) => {
        if (p.op_id === opId) onProgress(p);
      });
    }
    const abort = () => {
      void t.invoke(CANCEL_COMMAND, { opId }).catch(() => undefined);
    };
    if (opts.signal?.aborted) abort();
    opts.signal?.addEventListener('abort', abort, { once: true });
    try {
      return await call<T>(cmd, { ...args, opId });
    } finally {
      opts.signal?.removeEventListener('abort', abort);
      unlisten?.();
    }
  };

  return {
    kind: 'tauri',
    appInfo: () => call(COMMANDS.appInfo),
    drives: () => call(COMMANDS.drives),
    scan: (params, opts) => operation(COMMANDS.scan, { params }, opts),
    projects: () => call(COMMANDS.projects),
    worktrees: () => call(COMMANDS.worktrees),
    caches: (opts) => operation(COMMANDS.caches, {}, opts),
    status: () => call(COMMANDS.status),
    capabilities: () => call(COMMANDS.capabilities),
    history: () => call(COMMANDS.history),
    settings: () => call(COMMANDS.settings),
  };
}
