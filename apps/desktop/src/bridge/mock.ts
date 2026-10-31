// The mock bridge: used when the UI runs outside Tauri (plain browser, tests, screenshots).
// Never touches the filesystem.
//
// Its data is the REAL bridge's output: ./fixtures/*.json are written by the Rust test
// apps/desktop/bridge/tests/session.rs (SCULL_UPDATE_MOCKS=1) from a scan of a generated
// fixture tree, with paths rewritten to an imagined /Users/ada. A vitest test validates every
// file against the same JSON Schemas the TypeScript types come from, so mock and real answers
// have the same shape. Only the drives keep the design's illustrative machine.
import type {
  AppInfo,
  CachesReport,
  CapabilitiesReport,
  DesktopStatus,
  Drive,
  OperationOptions,
  ProgressEvent,
  ProjectsView,
  ScanParams,
  ScanReport,
  ScullBridge,
  WorktreesView,
} from './types';
import { BridgeError } from './types';
import scanReport from './fixtures/scan-report.json';
import projects from './fixtures/projects.json';
import worktrees from './fixtures/worktrees.json';
import cachesReport from './fixtures/caches-report.json';
import status from './fixtures/status.json';
import capabilities from './fixtures/capabilities.json';
import progressEvents from './fixtures/progress-events.json';

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

/** The fixtures, typed. Validated against the schemas in mock.test.ts. */
export const MOCK_DATA = {
  scanReport: scanReport as unknown as ScanReport,
  projects: projects as unknown as ProjectsView,
  worktrees: worktrees as unknown as WorktreesView,
  cachesReport: cachesReport as unknown as CachesReport,
  status: status as unknown as DesktopStatus,
  capabilities: capabilities as unknown as CapabilitiesReport,
  progressEvents: progressEvents as unknown as ProgressEvent[],
};

export interface MockOptions {
  /** Simulated latency per call, in milliseconds (0 in tests). */
  latencyMs?: number;
  /** Delay between simulated progress events, in milliseconds. */
  progressMs?: number;
}

export function createMockBridge(opts: MockOptions = {}): ScullBridge {
  const latency = opts.latencyMs ?? 0;
  const step = opts.progressMs ?? 0;
  let ops = 0;
  const wait = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));
  const later = async <T>(value: T): Promise<T> => {
    await wait(latency);
    return structuredClone(value);
  };

  /** Replay the recorded progress events under a fresh ID; abort -> cancelled report. */
  const operation = async <T extends { cancelled: boolean }>(
    result: T,
    o: OperationOptions = {},
  ): Promise<T> => {
    const opId = `mock-${++ops}`;
    for (const e of MOCK_DATA.progressEvents) {
      if (o.signal?.aborted) break;
      await wait(step);
      o.onProgress?.({ ...e, op_id: opId });
    }
    await wait(latency);
    const out = structuredClone(result);
    if (o.signal?.aborted) out.cancelled = true;
    return out;
  };

  return {
    kind: 'mock',
    appInfo: () => later<AppInfo>({ version: MOCK_DATA.capabilities.version, bridge: 'mock', os: 'macos' }),
    drives: () => later<Drive[]>([...MOCK_DRIVES]),
    scan: async (params: ScanParams, o?: OperationOptions) => {
      if (params.roots?.some((r) => !r.startsWith('/'))) {
        throw new BridgeError({ kind: 'usage', message: 'scan roots must be absolute paths' });
      }
      return operation(MOCK_DATA.scanReport, o);
    },
    projects: () => later(MOCK_DATA.projects),
    worktrees: () => later(MOCK_DATA.worktrees),
    caches: (o?: OperationOptions) => operation(MOCK_DATA.cachesReport, o),
    status: () => later(MOCK_DATA.status),
    capabilities: () => later(MOCK_DATA.capabilities),
  };
}
