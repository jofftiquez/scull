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
  FootprintHistory,
  FreeSpaceHistory,
  OperationOptions,
  ProgressEvent,
  ProjectsView,
  RegrowthHistory,
  RollupHistory,
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
import freeSpaceHistory from './fixtures/free-space-history.json';
import rollupHistory from './fixtures/rollup-history.json';
import regrowth from './fixtures/regrowth.json';
import footprintHistory from './fixtures/footprint-history.json';
import { withDesignWorktrees } from './worktreesMock';
import { designHistory } from './historyMock';
import { designSettings } from './settingsMock';
import { registerScanCandidates } from './planMock';

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
  // Recorded worktrees plus the design's illustrative ones (design 04), like MOCK_DRIVES.
  worktrees: withDesignWorktrees(worktrees as unknown as WorktreesView),
  cachesReport: cachesReport as unknown as CachesReport,
  status: status as unknown as DesktopStatus,
  capabilities: capabilities as unknown as CapabilitiesReport,
  progressEvents: progressEvents as unknown as ProgressEvent[],
  // Illustrative operations (design 09): this core records none yet (no Executor on main).
  history: designHistory(),
  // The design's machine (design 10): a config file with two roots, MCP on, one protected folder.
  settings: designSettings(),
};

/**
 * The recorded history (apps/desktop/bridge/tests/insights.rs) moved in time so its latest
 * timestamp is `now`: every `at`, `since` and `*_at` field shifts by the same amount.
 */
export function shiftHistory<T>(doc: T, now: number): T {
  const isTime = (k: string) => k === 'at' || k === 'since' || k.endsWith('_at');
  let latest = -Infinity;
  const scan = (v: unknown): void => {
    if (Array.isArray(v)) v.forEach(scan);
    else if (v && typeof v === 'object') {
      for (const [k, x] of Object.entries(v)) {
        if (isTime(k) && k !== 'since' && typeof x === 'number') latest = Math.max(latest, x);
        else scan(x);
      }
    }
  };
  scan(doc);
  if (!Number.isFinite(latest)) return structuredClone(doc);
  const delta = Math.floor(now) - latest;
  const shift = (v: unknown): unknown => {
    if (Array.isArray(v)) return v.map(shift);
    if (v && typeof v === 'object') {
      return Object.fromEntries(
        Object.entries(v).map(([k, x]) => [k, isTime(k) && typeof x === 'number' ? x + delta : shift(x)]),
      );
    }
    return v;
  };
  return shift(doc) as T;
}

/** Recorded history payloads (validated against their schemas in parity.test.ts). */
export const MOCK_HISTORY = {
  freeSpace: freeSpaceHistory as unknown as FreeSpaceHistory,
  rollups: rollupHistory as unknown as RollupHistory,
  regrowth: regrowth as unknown as RegrowthHistory,
  footprint: footprintHistory as unknown as FootprintHistory,
};

/** The latest timestamp of the recorded history, shared so all four series stay aligned. */
function historyLatest(): number {
  const ats = [
    ...MOCK_HISTORY.freeSpace.samples.map((s) => s.sampled_at),
    ...MOCK_HISTORY.footprint.samples.map((s) => s.at),
  ];
  return ats.length ? Math.max(...ats) : 0;
}

/** Shift a history payload to `now` and keep only entries at or after `since`. */
function historyAt<T extends { since: number }>(doc: T, now: number, sinceArg: number, keep: (d: T, since: number) => T): T {
  const since = Math.floor(sinceArg);
  const shifted = shiftHistory({ ...doc, anchor_at: historyLatest() }, now) as T & { anchor_at?: number };
  delete shifted.anchor_at;
  return keep({ ...shifted, since }, since);
}

export interface MockOptions {
  /** Simulated latency per call, in milliseconds (0 in tests). */
  latencyMs?: number;
  /** Delay between simulated progress events, in milliseconds. */
  progressMs?: number;
  /** Start as a machine that was never scanned (`state: "no_scan"`, no last scan) until the
   *  first complete scan, so the app opens on onboarding (`?scenario=first-run`, E2E). */
  firstRun?: boolean;
}

export function createMockBridge(opts: MockOptions = {}): ScullBridge {
  const latency = opts.latencyMs ?? 0;
  const step = opts.progressMs ?? 0;
  let ops = 0;
  let scanned = !opts.firstRun;
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
      const report = await operation(MOCK_DATA.scanReport, o);
      if (!report.cancelled) scanned = true;
      registerScanCandidates(report.candidates);
      return report;
    },
    projects: () => later(MOCK_DATA.projects),
    worktrees: () => later(MOCK_DATA.worktrees),
    caches: async (o?: OperationOptions) => {
      const report = await operation(MOCK_DATA.cachesReport, o);
      registerScanCandidates(report.candidates);
      return report;
    },
    status: () =>
      later<DesktopStatus>(scanned ? MOCK_DATA.status : { ...MOCK_DATA.status, state: 'no_scan', last_scan: null }),
    capabilities: () => later(MOCK_DATA.capabilities),
    history: () => later(MOCK_DATA.history),
    settings: () => later(MOCK_DATA.settings),
    freeSpaceHistory: (since: number, volumeId?: string) => {
      const now = Date.now() / 1000;
      const doc = historyAt(MOCK_HISTORY.freeSpace, now, since, (d, t) => ({
        ...d,
        samples: d.samples.filter((x) => x.sampled_at >= t),
      }));
      if (volumeId && volumeId !== doc.volume_id) return later({ ...doc, volume_id: volumeId, samples: [] });
      return later(doc);
    },
    rollupHistory: (since: number) =>
      later(
        historyAt(MOCK_HISTORY.rollups, Date.now() / 1000, since, (d, t) => ({
          ...d,
          series: d.series
            .map((c) => ({ ...c, points: c.points.filter((p) => p.at >= t) }))
            .filter((c) => c.points.some((p) => p.allocated_bytes > 0 || p.items > 0)),
        })),
      ),
    regrowth: (since: number) =>
      later(
        historyAt(MOCK_HISTORY.regrowth, Date.now() / 1000, since, (d, t) => ({
          ...d,
          events: d.events.filter((e) => e.reappeared_at >= t),
        })),
      ),
    footprintHistory: (since: number) =>
      later(
        historyAt(MOCK_HISTORY.footprint, Date.now() / 1000, since, (d, t) => ({
          ...d,
          samples: d.samples.filter((x) => x.at >= t),
        })),
      ),
  };
}
