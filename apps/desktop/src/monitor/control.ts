// Tray monitoring's controls (P5-PRO-01): app state, not files. Inside Tauri they reach the
// monitor thread (apps/desktop/src-tauri/src/monitor.rs) through MONITOR_COMMANDS; in a plain
// browser the mock keeps everything in memory. Neither deletes, moves or writes anything on
// disk: the monitor only reads and notifies.
import type { DigestView, MonitorConfig, MonitorSnapshot, MonitorStatus, ScullBridge } from '../bridge/types';
import { createTauriMonitorControl } from '../bridge/tauri';
import { mockMonitor } from './mock';

export interface MonitorControl {
  readonly kind: 'tauri' | 'mock';
  /** Send the user's settings (and, at start, the memory kept from the previous run). */
  configure(config: MonitorConfig): Promise<MonitorStatus>;
  /** Pause notifications for `secs` (at most 7 days), or resume with `null`. */
  pause(secs: number | null): Promise<MonitorStatus>;
  /** Show this week's digest as a notification now; the weekly slot doesn't move. */
  sendDigestNow(): Promise<DigestView>;
  /** Follow the monitor's snapshots (after checks, pauses) to keep them. */
  onSnapshot(handler: (s: MonitorSnapshot) => void): Promise<() => void>;
}

/** The control matching a bridge: the Tauri commands, or the in-memory mock. */
export function monitorControlFor(bridge: Pick<ScullBridge, 'kind'>): MonitorControl {
  return bridge.kind === 'tauri' ? createTauriMonitorControl() : mockMonitor.control;
}
