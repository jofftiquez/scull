import { reactive } from 'vue';
import { flushPromises, mount } from '@vue/test-utils';
import { describe, expect, it, vi } from 'vitest';
import { createMemoryHistory, createRouter } from 'vue-router';
import App from '../../App.vue';
import { BRIDGE_KEY, type CachesReport } from '../../bridge';
import { createMockBridge, MOCK_DATA } from '../../bridge/mock';
import { routes } from '../../navigation';
import { until } from '../../test/until';
import { createMockCacheActions, createTauriCacheActions, designPreview, previewFrom, MOCK_BINARIES } from './bridge';
import * as cacheBridge from './bridge';
import {
  commandLine,
  defaultMode,
  groupTotals,
  initialChoices,
  overall,
  rowProgress,
  runDuration,
  runSummary,
  selected,
  selectedBytes,
  skipText,
  toolOf,
} from './model';

const GiB = 1024 ** 3;
const { preview: demo, preselected } = designPreview();

/** The recorded npm cache, re-tiered S1 (as it is once in-use probes exist). */
function s1Report(): CachesReport {
  const r = structuredClone(MOCK_DATA.cachesReport);
  r.candidates[0]!.tier = 'S1';
  return r;
}

describe('Quick Clean preview', () => {
  it('skips the recorded S2 npm cache with the cleaner’s reason (D-28)', () => {
    const p = previewFrom(MOCK_DATA.cachesReport, MOCK_BINARIES, null);
    expect(p.tools).toHaveLength(0);
    expect(p.skipped).toHaveLength(1);
    expect(skipText(p.skipped[0]!.reason)).toBe('classified S2: tool commands are allowed at S0/S1 only');
  });

  it('shows the resolved binary and the tool’s own command, never a shell', () => {
    const p = previewFrom(s1Report(), MOCK_BINARIES, null);
    const npm = p.tools[0]!;
    expect(toolOf(MOCK_DATA.cachesReport.candidates[0]!)).toBe('npm');
    expect(npm.binary).toBe('/opt/homebrew/bin/npm');
    expect(commandLine(npm, 'prune')).toBe('/opt/homebrew/bin/npm cache verify');
    expect(commandLine(npm, 'wipe')).toBe('/opt/homebrew/bin/npm cache clean --force');
    expect(defaultMode(npm)).toBe('prune');
  });

  it('turns a cache without a trusted binary into an orphan the executor removes (D-36)', () => {
    const p = previewFrom(s1Report(), {}, null);
    expect(p.tools[0]!.runner).toBe('executor');
    expect(commandLine(p.tools[0]!, 'wipe')).toBe('removed safely by scull');
    // Orphans start unticked: removing one needs a per-tool confirmation.
    expect(initialChoices(p.tools)[p.tools[0]!.item_id]).toEqual({ on: false, mode: 'wipe' });
  });

  it('explains every skip reason, incl. Windows .cmd shims (#218)', () => {
    expect(skipText({ kind: 'shell_shim_only', shim: 'C:\\npm\\pnpm.cmd' })).toBe(
      'only C:\\npm\\pnpm.cmd found, which needs a shell to run (Windows shims: #218)',
    );
    expect(skipText({ kind: 'not_installed', manual_removal: '~/.gradle/caches' })).toContain('left in place for manual removal');
    expect(skipText({ kind: 'in_use', by: ['a', 'b'] })).toBe('in use (a; b)');
    expect(skipText({ kind: 'elevated' })).toContain('SAFE-14');
    expect(demo.skipped.map((s) => s.reason.kind)).toEqual(['shell_shim_only', 'in_use', 'not_installed']);
  });

  it('pnpm has no wipe, Bun has no prune', () => {
    const pnpm = demo.tools.find((t) => t.tool === 'pnpm')!;
    const bun = demo.tools.find((t) => t.tool === 'bun')!;
    expect(pnpm.wipe).toBeNull();
    expect(bun.prune).toBeNull();
    expect(defaultMode(bun)).toBe('wipe');
  });

  it('inside Tauri: uses native commands, selected modes and measured outcomes', async () => {
    const prune = { binary: '/trusted/npm', args: ['cache', 'verify'], env: [{ name: 'npm_config_cache', value: '/fake/cache' }] };
    const wipe = { ...prune, args: ['cache', 'clean', '--force'] };
    const calls: { command: string; args?: Record<string, unknown> }[] = [];
    const native = {
      tools: [{ item_id: 'cache-1', tool: 'npm', name: 'npm cache', cache: '/fake/cache', bytes: 4096, prune, wipe, notes: [] }],
      skipped: [{ tool: 'pip', name: 'pip cache', path: '/fake/pip', bytes: 100, reason: 'not supported yet' }],
    };
    let answer: unknown = native;
    const actions = createTauriCacheActions({
      invoke: async <T,>(command: string, args?: Record<string, unknown>): Promise<T> => {
        calls.push({ command, args });
        return answer as T;
      },
      listen: async () => () => undefined,
    });
    const preview = reactive(await actions.plan(s1Report()));
    expect(actions.kind).toBe('tauri');
    expect(preview.unavailable).toBeNull();
    expect(commandLine(preview.tools[0]!, 'prune')).toBe('npm_config_cache=/fake/cache /trusted/npm cache verify');
    expect(skipText(preview.skipped[0]!.reason)).toBe('not supported yet');
    answer = { operation_id: 'op-1', tools: [{ item_id: 'cache-1', tool: 'npm', succeeded: true, cache_bytes_freed: 1024 }], skipped: [] };
    const result = await actions.run(preview, [{ item_id: 'cache-1', mode: 'wipe' }]);
    expect(calls).toEqual([
      { command: 'quick_clean_plan', args: undefined },
      { command: 'quick_clean_run', args: { choices: [{ item_id: 'cache-1', mode: 'wipe', command: wipe }] } },
    ]);
    expect(result[0]).toMatchObject({ ok: true, freed_bytes: 1024, error: null });
    answer = { operation_id: 'op-2', tools: [], skipped: [{ tool: 'npm', reason: 'in use' }] };
    expect((await actions.run(preview, [{ item_id: 'cache-1', mode: 'prune' }]))[0]).toMatchObject({ ok: false, error: 'in use', freed_bytes: 0 });
    await expect(actions.run(preview, [{ item_id: 'unknown', mode: 'prune' }])).rejects.toThrow(/unavailable/);
    await expect(actions.run({ ...preview }, [{ item_id: 'cache-1', mode: 'wipe' }])).rejects.toThrow(/Refresh/);
  });
});

describe('selection and totals', () => {
  it('adds up the selection like the design (4 caches · 14.9)', () => {
    const c = initialChoices(demo.tools);
    for (const t of demo.tools) c[t.item_id]!.on = preselected.includes(t.item_id);
    expect(selected(demo.tools, c).map((t) => t.tool)).toEqual(['npm', 'pnpm', 'bun', 'yarn']);
    expect(selectedBytes(demo.tools, c) / GiB).toBeCloseTo(14.9, 1);
  });

  it('totals groups, counting skipped caches', () => {
    const g = groupTotals(demo.tools, demo.skipped);
    expect(g.map((x) => x.group)).toEqual(['Package managers', 'Models', 'Containers & system', 'Test browsers']);
    expect(g[2]!.bytes / GiB).toBeCloseTo(20.3, 1);
    expect(g[3]!.bytes / GiB).toBeCloseTo(2.7, 1);
  });
});

describe('sequential run', () => {
  it('fills one row at a time', () => {
    expect(rowProgress(0, 4, 0.1)).toEqual({ state: 'active', pct: 40 });
    expect(rowProgress(1, 4, 0.1)).toEqual({ state: 'queued', pct: 0 });
    expect(rowProgress(0, 4, 0.3)).toEqual({ state: 'done', pct: 100 });
    expect(rowProgress(1, 4, 0.3)).toEqual({ state: 'active', pct: 20 });
    expect(rowProgress(3, 4, 1)).toEqual({ state: 'done', pct: 100 });
    expect(rowProgress(-1, 4, 0.5)).toEqual({ state: 'idle', pct: 0 });
    expect(overall(1, 0.5, 4)).toBe(0.375);
    expect(runDuration(4)).toBe(2900);
  });

  it('the mock reports progress in order and measures a bit less than planned', async () => {
    const a = createMockCacheActions({ msPerTool: 0 });
    const run = preselected.map((id) => ({ item_id: id, mode: 'prune' as const }));
    const seen: number[] = [];
    const res = await a.run(demo, run, (i) => seen.push(i));
    expect(seen.at(-1)).toBe(3);
    const s = runSummary(res, 100);
    expect(s.count).toBe(4);
    expect(s.measured).toBeLessThan(selectedBytes(demo.tools, Object.fromEntries(preselected.map((id) => [id, { on: true, mode: 'prune' }]))));
    expect(res.find((r) => r.tool === 'pnpm')!.explanation).toMatch(/QC-06/);
  });
});

async function mountAt(path: string) {
  const router = createRouter({ history: createMemoryHistory(), routes });
  router.push(path);
  await router.isReady();
  const w = mount(App, { global: { plugins: [router], provide: { [BRIDGE_KEY as symbol]: createMockBridge() } } });
  // Wait for the caches report (rows or the skipped row), not a fixed time.
  await vi.waitUntil(
    async () => {
      await flushPromises();
      return w.find('[data-testid="cache-row"]').exists() || w.find('[data-testid="skipped-row"]').exists();
    },
    { timeout: 5000 },
  );
  return w;
}

describe('caches screen', () => {
  it('does not claim zero cache bytes while live completion is awaiting the measured result', async () => {
    const preview = { ...designPreview().preview, tools: [designPreview().preview.tools[0]!], skipped: [] };
    let release!: () => void;
    const gate = new Promise<void>((resolve) => { release = resolve; });
    const factory = vi.spyOn(cacheBridge, 'cacheActionsFor').mockReturnValue({
      kind: 'tauri', plan: async () => preview,
      run: async (_preview, _choices, _progress, event) => {
        event?.({ op_id: 'fixture', kind: 'finished', item_id: 'demo-npm', tool: 'npm', succeeded: true, completed: 1, total: 1 });
        await gate;
        return [{ tool: 'npm', item_id: 'demo-npm', ok: true, freed_bytes: 0, explanation: null, error: null }];
      },
    });
    const w = await mountAt('/caches');
    try {
      await w.get('[data-testid="start"]').trigger('click');
      await w.get('[data-testid="confirm-action"]').trigger('click');
      await flushPromises();
      expect(w.get('[data-testid="row-size"]').text()).toBe('6.1 GiB');
      expect(w.get('[data-testid="row-state"]').text()).toBe('completed ✓');
      release(); await flushPromises();
    } finally { release(); w.unmount(); factory.mockRestore(); }
  });
  it('retains a successfully pruned cache when nothing was freed and refreshes it for another mode', async () => {
    const fresh = () => ({ ...designPreview().preview, tools: [designPreview().preview.tools[0]!], skipped: [] });
    const plan = vi.fn(async () => fresh());
    const factory = vi.spyOn(cacheBridge, 'cacheActionsFor').mockReturnValue({
      kind: 'tauri', plan,
      run: async () => [{ tool: 'npm', item_id: 'demo-npm', ok: true, freed_bytes: 0, explanation: null, error: null }],
    });
    const w = await mountAt('/caches');
    try {
      await w.get('[data-testid="start"]').trigger('click');
      await w.get('[data-testid="confirm-action"]').trigger('click');
      await flushPromises();
      expect(w.get('[data-testid="row-size"]').text()).toBe('6.1 GiB');
      expect(w.get('[data-testid="main-footer"]').text()).toContain('0 B freed');
      await w.get('[data-testid="start"]').trigger('click');
      await flushPromises();
      expect(plan).toHaveBeenCalledTimes(2);
      expect(w.get('input[aria-label="Include npm"]').attributes('disabled')).toBeUndefined();
      expect(w.get('[data-testid="row-size"]').text()).toBe('6.1 GiB');
      expect(w.get('[data-testid="start"]').attributes('disabled')).toBeUndefined();
    } finally { w.unmount(); factory.mockRestore(); }
  });
  it('shows the real report: the S2 npm cache skipped, nothing to clean', async () => {
    const w = await mountAt('/caches');
    expect(w.findAll('[data-testid="cache-row"]')).toHaveLength(0);
    expect(w.get('[data-testid="skipped-row"]').text()).toContain('classified S2');
    expect(w.get('[data-testid="start"]').attributes('disabled')).toBeDefined();
    expect(w.get('[data-testid="not-undoable"]').text()).toContain('Not undoable');
  });

  it('runs the design selection sequentially to the done state (08b)', async () => {
    const w = await mountAt('/caches?demo=design');
    expect(w.get('[data-testid="start"]').text()).toBe('Clean caches');
    expect(w.get('[data-testid="row-cmd"]').text()).toContain('/opt/homebrew/bin/npm cache verify');
    // The simulated run maps wall-clock time to progress (650 ms per tool), so on a loaded
    // machine one late timer can carry it past a whole row before the test looks (#290). The
    // test owns `Date` instead: time stands still at each step and moves only when told to.
    const root = w.element.parentNode!;
    const states = () => w.findAll('[data-testid="row-state"]').map((s) => s.text()).join(' | ');
    vi.useFakeTimers({ toFake: ['Date'] });
    try {
      await w.get('[data-testid="start"]').trigger('click');
      await w.get('[data-testid="confirm-action"]').trigger('click');
      await until(() => states() === 'cleaning… | queued | queued | queued', root);
      // Halfway through the second tool.
      vi.setSystemTime(Date.now() + runDuration(4) * 0.375);
      await until(() => states() === 'cleaned ✓ | cleaning… | queued | queued', root);
      vi.setSystemTime(Date.now() + runDuration(4));
      await until(() => w.get('[data-testid="main-footer"]').text().includes('14.4 GiB freed'), root);
    } finally {
      vi.useRealTimers();
    }
    expect(w.get('[data-testid="main-footer"]').text()).toContain('14.4 GiB freed');
    expect(w.findAll('[data-testid="row-state"]').map((s) => s.text())).toEqual(['cleaned ✓', 'cleaned ✓', 'cleaned ✓', 'cleaned ✓']);
  });

  it('asks before including an orphaned cache', async () => {
    const w = await mountAt('/caches?demo=design');
    const box = w.get('input[aria-label="Include Playwright browsers"]');
    await box.setValue(true);
    expect((box.element as HTMLInputElement).checked).toBe(false);
    expect(w.get('[data-testid="orphan-confirm"]').text()).toContain('Not undoable');
    await w.get('[data-testid="orphan-confirm"] .btn-ink').trigger('click');
    expect((w.get('input[aria-label="Include Playwright browsers"]').element as HTMLInputElement).checked).toBe(true);
    expect(w.get('[data-testid="start"]').text()).toBe('Clean caches');
  });
});
