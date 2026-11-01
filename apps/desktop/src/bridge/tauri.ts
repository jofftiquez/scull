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
  type DigestView,
  type MonitorConfig,
  type MonitorSnapshot,
  type MonitorStatus,
  type OperationOptions,
  type ProgressEvent,
  type ScullBridge,
} from './types';
import type { MonitorControl } from '../monitor/control';

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
    freeSpaceHistory: (since, volumeId) => call(COMMANDS.freeSpaceHistory, { since, volumeId: volumeId ?? null }),
    rollupHistory: (since) => call(COMMANDS.rollupHistory, { since }),
    regrowth: (since) => call(COMMANDS.regrowth, { since }),
    footprintHistory: (since) => call(COMMANDS.footprintHistory, { since }),
    weeklyDigest: () => call(COMMANDS.weeklyDigest),
    monitorStatus: () => call(COMMANDS.monitorStatus),
  };
}

/**
 * The menu bar widget's window commands (apps/desktop/src-tauri/src/tray.rs). They move windows,
 * not data, so they are not on ScullBridge; none of them reaches the filesystem.
 */
export const WINDOW_COMMANDS = {
  /** Show and focus the main window on one of the allowed routes, and hide the widget. */
  openMain: 'open_main_window',
  /** Hide the widget window. */
  hideWidget: 'hide_menu_bar',
} as const;

/** The event the main window receives with the route to show. */
export const NAVIGATE_EVENT = 'scull://navigate';

export interface TauriWindowCommands {
  openMain(route: string): Promise<void>;
  hideWidget(): Promise<void>;
  /** Main window: follow `open_main_window` requests. Resolves to the unsubscribe function. */
  onNavigate(handler: (route: string) => void): Promise<UnlistenFn>;
}

export function createTauriWindowCommands(t: TauriTransport = realTransport): TauriWindowCommands {
  const call = async (cmd: string, args?: Record<string, unknown>): Promise<void> => {
    try {
      await t.invoke<void>(cmd, args);
    } catch (e) {
      throw toBridgeError(e);
    }
  };
  return {
    openMain: (route) => call(WINDOW_COMMANDS.openMain, { route }),
    hideWidget: () => call(WINDOW_COMMANDS.hideWidget),
    onNavigate: (handler) => t.listen<string>(NAVIGATE_EVENT, handler),
  };
}

/**
 * Tray monitoring's commands (apps/desktop/src-tauri/src/monitor.rs). They change the
 * monitor's in-memory settings or show a notification, never a file, so they are not on
 * ScullBridge.
 */
export const MONITOR_COMMANDS = {
  configure: 'configure_monitor',
  pause: 'pause_monitor',
  sendDigestNow: 'send_digest_now',
} as const;

/** The event carrying MonitorSnapshot payloads for the main window to keep. */
export const MONITOR_EVENT = 'scull://monitor';

export function createTauriMonitorControl(t: TauriTransport = realTransport): MonitorControl {
  const call = async <T>(cmd: string, args?: Record<string, unknown>): Promise<T> => {
    try {
      return await t.invoke<T>(cmd, args);
    } catch (e) {
      throw toBridgeError(e);
    }
  };
  return {
    kind: 'tauri',
    configure: (config: MonitorConfig) => call<MonitorStatus>(MONITOR_COMMANDS.configure, { config }),
    pause: (secs: number | null) => call<MonitorStatus>(MONITOR_COMMANDS.pause, { secs }),
    sendDigestNow: () => call<DigestView>(MONITOR_COMMANDS.sendDigestNow),
    onSnapshot: (handler: (s: MonitorSnapshot) => void) => t.listen<MonitorSnapshot>(MONITOR_EVENT, handler),
  };
}
