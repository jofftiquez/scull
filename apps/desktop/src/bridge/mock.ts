// The mock bridge: used when the UI runs outside Tauri (plain browser, tests, screenshots).
// Never touches the filesystem.
//
// Its data is the REAL bridge's output: ./fixtures/*.json are written by the Rust test
// apps/desktop/bridge/tests/session.rs (SCULL_UPDATE_MOCKS=1) from a scan of a generated
// fixture tree, with paths rewritten to an imagined /Users/ada. A vitest test validates every
// file against the same JSON Schemas the TypeScript types come from, so mock and real answers
// have the same shape.
import type { CachesReport, OperationOptions, ProgressEvent, ScanParams, ScanReport, ScullBridge } from './types';
import { BridgeError } from './types';
import scanReport from './fixtures/scan-report.json';
import cachesReport from './fixtures/caches-report.json';
import progressEvents from './fixtures/progress-events.json';
import { designSettings } from './settingsMock';
import { registerScanCandidates } from './planMock';

/** The fixtures, typed. Validated against the schemas in parity.test.ts. */
export const MOCK_DATA = {
  scanReport: scanReport as unknown as ScanReport,
  cachesReport: cachesReport as unknown as CachesReport,
  progressEvents: progressEvents as unknown as ProgressEvent[],
  // The design's machine: a config file with roots, MCP on, one protected folder.
  settings: designSettings(),
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

  /** Replay the recorded progress events under a fresh ID; abort -> cancelled report. */
  const operation = async <T extends { cancelled: boolean }>(result: T, o: OperationOptions = {}): Promise<T> => {
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
    settings: async () => {
      await wait(latency);
      return structuredClone(MOCK_DATA.settings);
    },
    scan: async (params: ScanParams, o?: OperationOptions) => {
      if (params.roots?.some((r) => !r.startsWith('/'))) {
        throw new BridgeError({ kind: 'usage', message: 'scan roots must be absolute paths' });
      }
      const report = await operation(MOCK_DATA.scanReport, o);
      registerScanCandidates(report.candidates);
      return report;
    },
    caches: async (o?: OperationOptions) => {
      const report = await operation(MOCK_DATA.cachesReport, o);
      registerScanCandidates(report.candidates);
      return report;
    },
  };
}
