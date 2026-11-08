import { beforeEach, describe, expect, it } from 'vitest';
import { createMockBridge, MOCK_DATA } from '../bridge/mock';
import { BridgeError, type ScullBridge } from '../bridge';
import { ago, cancelScan, core, refresh, rescan, statusLine } from './core';
import { saveSetup } from '../components/onboarding/prefs';
import { defaultAnswers } from '../components/onboarding/model';
import { MOCK_DETECTION } from '../components/onboarding/bridge';
import { NOT_SCANNED, shell } from './shell';

beforeEach(() => {
  localStorage.clear();
  Object.assign(core, { status: null, scanning: false, progress: null, error: null });
  shell.counts = {};
  shell.status = NOT_SCANNED;
});

describe('status line', () => {
  it('reads like the design', () => {
    expect(ago(30)).toBe('just now');
    expect(ago(240)).toBe('4 min ago');
    expect(ago(7200)).toBe('2 h ago');
    expect(ago(86400)).toBe('1 day ago');
    expect(statusLine(core)).toBe(NOT_SCANNED);
    core.status = structuredClone(MOCK_DATA.status);
    core.status.last_scan = { ...core.status.last_scan!, age_secs: 240, roots: 3, reclaimable_bytes: 2048 };
    expect(statusLine(core)).toBe('scanned 4 min ago · 3 folders · 2.0 KiB reclaimable');
  });

  it('shows live progress while scanning', () => {
    core.scanning = true;
    core.progress = { ...MOCK_DATA.progressEvents[0]!, phase: 'scanning', entries: 412880, allocated_bytes: 1024 ** 3 };
    expect(statusLine(core)).toBe('scanning · 412,880 entries · 1.0 GiB found');
    core.progress = { ...core.progress, phase: 'analyzing', what: 'classifying' };
    expect(statusLine(core)).toBe('analysing · classifying');
  });
});

describe('refresh and rescan', () => {
  it('uses native config roots after they are saved instead of the old survey roots', async () => {
    const base = createMockBridge();
    const answers = { ...defaultAnswers(MOCK_DETECTION), roots: ['/Users/ada/old'], connectAgents: false };
    saveSetup({ version: 1, home: '/Users/ada', answers });
    const settings = await base.settings();
    settings.origins['scan.roots'] = [{ source: 'user_config' }];
    settings.config.scan!.roots = [];
    let requested: unknown;
    const bridge: ScullBridge = { ...base, kind: 'tauri', settings: async () => settings,
      scan: async (params) => { requested = params; return MOCK_DATA.scanReport; } };
    await rescan(bridge);
    expect(requested).toEqual({});
  });
  it('rescans saved desktop folders and falls back to core config without a saved setup', async () => {
    const requests: unknown[] = [];
    const base = createMockBridge();
    const b: ScullBridge = { ...base, scan: async (params) => { requests.push(params); return MOCK_DATA.scanReport; } };
    await rescan(b);
    const answers = { ...defaultAnswers(MOCK_DETECTION), roots: ['/Users/ada/Projects'], connectAgents: false };
    saveSetup({ version: 1, home: '/Users/ada', answers });
    await rescan(b);
    expect(requests).toEqual([{}, { roots: ['/Users/ada/Projects'], allow_broad_roots: false }]);
  });

  it('fills the header and the nav figures from the bridge', async () => {
    await refresh(createMockBridge());
    expect(shell.status).toMatch(/^scanned /);
    expect(shell.counts.projects).toBe(String(MOCK_DATA.projects.projects.length));
    expect(shell.counts.worktrees).toBe(String(MOCK_DATA.worktrees.worktrees.length));
  });

  it('streams progress into the header, then refreshes', async () => {
    const seen: string[] = [];
    const base = createMockBridge();
    const b: ScullBridge = {
      ...base,
      scan: (p, o) =>
        base.scan(p, {
          ...o,
          onProgress: (e) => {
            o?.onProgress?.(e);
            seen.push(shell.status);
          },
        }),
    };
    await rescan(b);
    expect(seen.some((s) => s.startsWith('scanning ·'))).toBe(true);
    expect(core.scanning).toBe(false);
    expect(shell.status).toMatch(/^scanned /);
  });

  it('cancel aborts the running scan', async () => {
    let aborted = false;
    const base = createMockBridge();
    const b: ScullBridge = {
      ...base,
      scan: (p, o) =>
        new Promise((resolve) => {
          o?.signal?.addEventListener('abort', () => {
            aborted = true;
            resolve({ ...MOCK_DATA.scanReport, cancelled: true });
          });
        }),
    };
    const running = rescan(b);
    expect(core.scanning).toBe(true);
    cancelScan();
    await running;
    expect(aborted).toBe(true);
    expect(core.scanning).toBe(false);
  });

  it('a failed scan shows the core error', async () => {
    const b: ScullBridge = {
      ...createMockBridge(),
      scan: () => Promise.reject(new BridgeError({ kind: 'usage', message: 'no scan roots' })),
    };
    await rescan(b);
    expect(core.error).toBe('no scan roots');
    expect(shell.status).toBe('scan failed · no scan roots');
  });
});
