// The mock bridge: answers like the real one, with the design's illustrative machine (one
// imagined Mac, design/README.md). Used when the UI runs outside Tauri (plain browser, tests,
// screenshots). Never touches the filesystem.
import type { AppInfo, Drive, ScullBridge } from './types';

const GiB = 1024 ** 3;

/** The design's drives: Macintosh HD (92% used), an external Dev SSD, a Time Machine disk. */
export const MOCK_DRIVES: readonly Drive[] = [
  {
    mount_point: '/',
    label: 'Macintosh HD',
    kind: 'local',
    scan_policy: 'scan',
    holds_home: true,
    total_bytes: Math.round(494 * GiB),
    free_bytes: Math.round(41.2 * GiB),
  },
  {
    mount_point: '/Volumes/Dev SSD',
    label: 'Dev SSD',
    kind: 'removable',
    scan_policy: 'scan',
    holds_home: false,
    total_bytes: Math.round(1000 * GiB),
    free_bytes: Math.round(390 * GiB),
  },
  {
    mount_point: '/Volumes/Backup',
    label: 'Backup (Time Machine)',
    kind: 'backup',
    scan_policy: 'never',
    holds_home: false,
    total_bytes: Math.round(2000 * GiB),
    free_bytes: Math.round(820 * GiB),
  },
];

export interface MockOptions {
  /** Simulated latency per call, in milliseconds (0 in tests). */
  latencyMs?: number;
}

export function createMockBridge(opts: MockOptions = {}): ScullBridge {
  const latency = opts.latencyMs ?? 0;
  const later = <T>(value: T): Promise<T> =>
    new Promise((resolve) => setTimeout(() => resolve(structuredClone(value)), latency));
  return {
    kind: 'mock',
    appInfo: () => later<AppInfo>({ version: '0.0.0', bridge: 'mock', os: 'macos' }),
    drives: () => later<Drive[]>([...MOCK_DRIVES]),
  };
}
