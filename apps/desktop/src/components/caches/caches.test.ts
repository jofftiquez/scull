import { flushPromises, mount } from '@vue/test-utils';
import { describe, expect, it } from 'vitest';
import { createMemoryHistory, createRouter } from 'vue-router';
import App from '../../App.vue';
import { BRIDGE_KEY, type CachesReport } from '../../bridge';
import { createMockBridge, MOCK_DATA } from '../../bridge/mock';
import { routes } from '../../navigation';
import { createMockCacheActions, createPendingCacheActions, designPreview, previewFrom, MOCK_BINARIES, PENDING_REASON } from './bridge';
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

  it('inside Tauri: planned from the report, but not runnable yet', async () => {
    const a = createPendingCacheActions();
    const p = await a.plan(s1Report());
    expect(p.unavailable).toBe(PENDING_REASON);
    expect(p.tools[0]!.binary).toBeNull();
    await expect(a.run(p, [])).rejects.toThrow(/quick_clean_run/);
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
  await new Promise((r) => setTimeout(r, 300));
  await flushPromises();
  return w;
}

describe('caches screen', () => {
  it('shows the real report: the S2 npm cache skipped, nothing to clean', async () => {
    const w = await mountAt('/caches');
    expect(w.findAll('[data-testid="cache-row"]')).toHaveLength(0);
    expect(w.get('[data-testid="skipped-row"]').text()).toContain('classified S2');
    expect(w.get('[data-testid="start"]').attributes('disabled')).toBeDefined();
    expect(w.get('[data-testid="not-undoable"]').text()).toContain('Not undoable');
  });

  it('runs the design selection sequentially to the done state (08b)', async () => {
    const w = await mountAt('/caches?demo=design');
    expect(w.get('[data-testid="start"]').text()).toBe('Clean 4 caches · 14.9 GiB');
    expect(w.get('[data-testid="row-cmd"]').text()).toContain('/opt/homebrew/bin/npm cache verify');
    await w.get('[data-testid="start"]').trigger('click');
    await new Promise((r) => setTimeout(r, 400));
    await flushPromises();
    const states = w.findAll('[data-testid="row-state"]').map((s) => s.text());
    expect(states).toEqual(['cleaning…', 'queued', 'queued', 'queued']);
    await new Promise((r) => setTimeout(r, 2800));
    await flushPromises();
    expect(w.get('[data-testid="done"]').text()).toContain('+14.9 GiB');
    expect(w.get('[data-testid="done"]').text()).toContain('freed from 4 caches');
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
    expect(w.get('[data-testid="start"]').text()).toBe('Clean 5 caches · 16.8 GiB');
  });
});
