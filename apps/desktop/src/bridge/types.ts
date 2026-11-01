// The desktop's view of the scull core. Every screen talks to a ScullBridge, never to Tauri
// directly, so the UI runs in a plain browser with the mock bridge (development, screenshots,
// tests) and inside Tauri with the real one.
//
// Payload types are GENERATED from the Rust types the commands return (./generated.ts, from
// apps/desktop/schemas). Shared types (ScanReport, Candidate, CachesReport, ScullError, ...) are
// the CLI's: tests check the desktop schemas equal crates/scull-cli/schemas.
//
// The bridge is read-only by design (SAFE-10): it has no call that deletes, moves or modifies
// a file. Cleanups go through the core Executor behind a reviewed plan, never through here.
import type {
  AppInfo,
  CachesReport,
  CapabilitiesReport,
  DesktopStatus,
  Drive,
  FootprintHistory,
  FreeSpaceHistory,
  HistoryView,
  ProgressEvent,
  ProjectsView,
  RegrowthHistory,
  RollupHistory,
  ScanParams,
  ScanReport,
  ScullError,
  SettingsView,
  WorktreesView,
} from './generated';

export type * from './generated';

/** Options of a long-running operation (scan, caches). */
export interface OperationOptions {
  /** Progress, throttled by the core (at most ~10 per second). */
  onProgress?: (event: ProgressEvent) => void;
  /** Aborting cancels the operation; it then resolves with a report marked `cancelled`. */
  signal?: AbortSignal;
}

/** Every bridge call rejects with this: the core's ScullError (same JSON as the CLI's). */
export class BridgeError extends Error {
  readonly error: ScullError;
  constructor(error: ScullError) {
    super(error.message);
    this.name = 'BridgeError';
    this.error = error;
  }
  get kind(): ScullError['kind'] {
    return this.error.kind;
  }
}

/** Whether a value is a ScullError payload. */
export function isScullError(v: unknown): v is ScullError {
  return (
    typeof v === 'object' &&
    v !== null &&
    typeof (v as { kind?: unknown }).kind === 'string' &&
    typeof (v as { message?: unknown }).message === 'string'
  );
}

/** Wrap anything thrown by a transport into a BridgeError. */
export function toBridgeError(e: unknown): BridgeError {
  if (e instanceof BridgeError) return e;
  if (isScullError(e)) return new BridgeError(e);
  return new BridgeError({ kind: 'internal', message: e instanceof Error ? e.message : String(e) });
}

export interface ScullBridge {
  readonly kind: 'tauri' | 'mock';
  appInfo(): Promise<AppInfo>;
  /** Mounted volumes worth showing: the home drive first, then other scanned drives, then
   * the ones scull does not scan (backup, network), each once. */
  drives(): Promise<Drive[]>;
  /** Scan and classify (read-only). Roots default to `scan.roots` from the config. */
  scan(params: ScanParams, opts?: OperationOptions): Promise<ScanReport>;
  /** Projects of the last complete scan (empty before the first). */
  projects(): Promise<ProjectsView>;
  /** Git worktrees of the last complete scan. */
  worktrees(): Promise<WorktreesView>;
  /** Package-manager caches (Quick Clean candidates). */
  caches(opts?: OperationOptions): Promise<CachesReport>;
  /** Last recorded scan and free-space samples. */
  status(): Promise<DesktopStatus>;
  /** What this machine and build can observe. */
  capabilities(): Promise<CapabilitiesReport>;
  /** Recorded cleanup operations with their audit logs, newest first (read-only: restoring
   * is not a bridge call, see components/history/bridge.ts). */
  history(): Promise<HistoryView>;
  /** Effective configuration with the origin of every value, built-in protection and scull's
   * own data (read-only: changing a setting is not a bridge call, see
   * components/settings/bridge.ts). */
  settings(): Promise<SettingsView>;
  /** Free-space samples of one volume since `since` (Unix seconds). The volume defaults to
   * the home folder's, then `/`, then the most recently sampled one. Read-only. */
  freeSpaceHistory(since: number, volumeId?: string): Promise<FreeSpaceHistory>;
  /** Bytes per category per usable scan since `since`. Read-only. */
  rollupHistory(since: number): Promise<RollupHistory>;
  /** Artifacts that came back since `since`. Read-only. */
  regrowth(since: number): Promise<RegrowthHistory>;
  /** scull's own footprint samples since `since`. Read-only. */
  footprintHistory(since: number): Promise<FootprintHistory>;
}

/** Bridge methods and the Tauri command each calls (also used by the parity test). */
export const COMMANDS = {
  appInfo: 'app_info',
  drives: 'drives',
  scan: 'scan',
  projects: 'projects',
  worktrees: 'worktrees',
  caches: 'caches',
  status: 'status',
  capabilities: 'capabilities',
  history: 'history',
  settings: 'settings',
  freeSpaceHistory: 'free_space_history',
  rollupHistory: 'rollup_history',
  regrowth: 'regrowth',
  footprintHistory: 'footprint_history',
} as const satisfies Record<Exclude<keyof ScullBridge, 'kind'>, string>;

/** The Tauri command that cancels an operation. */
export const CANCEL_COMMAND = 'cancel_operation';

/** The Tauri event carrying ProgressEvent payloads. */
export const PROGRESS_EVENT = 'scull://progress';
