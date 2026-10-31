// The desktop's view of the scull core. Every screen talks to a ScullBridge, never to Tauri
// directly, so the UI runs in a plain browser with the mock bridge (development, screenshots,
// tests) and inside Tauri with the real one. Field names match the Rust side (snake_case).
//
// The bridge is read-only by design (SAFE-10): it has no call that deletes, moves or modifies
// a file. Cleanups go through the core Executor behind an approval, never through this
// interface.

/** How scull treats a volume (D-22): scanned, scanned only on request, never scanned. */
export type ScanPolicy = 'scan' | 'off_by_default' | 'never';

/** What kind of storage a volume is (mirrors `scull_platform::VolumeKind`). */
export type DriveKind = 'local' | 'removable' | 'network' | 'virtual' | 'backup';

/** One mounted volume, for the drives widget. Unknown sizes are `null`, never 0. */
export interface Drive {
  /** Where it is mounted: `/`, `/Volumes/Dev SSD`, `C:\`. */
  mount_point: string;
  /** Short display name. */
  label: string;
  kind: DriveKind;
  scan_policy: ScanPolicy;
  /** The volume holding the user's home folder. Listed first. */
  holds_home: boolean;
  total_bytes: number | null;
  /** Bytes available to the current user. */
  free_bytes: number | null;
}

/** Which bridge is answering, and about what. */
export interface AppInfo {
  /** scull version of the core the bridge talks to. */
  version: string;
  /** `tauri` inside the app; `mock` in a plain browser. */
  bridge: 'tauri' | 'mock';
  /** The operating system the core runs on. */
  os: 'macos' | 'windows' | 'linux' | 'unknown';
}

export interface ScullBridge {
  readonly kind: 'tauri' | 'mock';
  appInfo(): Promise<AppInfo>;
  /** Mounted volumes worth showing: local and removable drives first, then the ones scull
   * does not scan (backup, network), each once. */
  drives(): Promise<Drive[]>;
}
