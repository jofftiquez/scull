import { flushPromises, mount } from '@vue/test-utils';
import { describe, expect, it, vi } from 'vitest';
import { createMemoryHistory, createRouter } from 'vue-router';
import App from '../../App.vue';
import { BRIDGE_KEY, type ProjectSummary, type ScullBridge } from '../../bridge';
import { createMockBridge, MOCK_DATA } from '../../bridge/mock';
import { routes } from '../../navigation';
import { createMockProjectActions, createPendingProjectActions, designProjects } from './bridge';
import {
  ALL,
  avatar,
  canClean,
  filters,
  idleClean,
  inside,
  lastActivity,
  matches,
  projectSize,
  shortPath,
  shrink,
  sortBySize,
  tierBar,
} from './model';

const GiB = 1024 ** 3;
const design = designProjects(1_000_000_000).projects;
const byName = (n: string) => design.find((p) => p.name === n)!;

describe('filters', () => {
  it('lists All, then each ecosystem present, biggest first', () => {
    const f = filters(design);
    expect(f[0]).toEqual({ id: ALL, label: 'All', count: 8 });
    expect(f.map((x) => x.label)).toEqual(['All', 'Rust', 'JS/TS', 'Docker', 'ML', 'Python', 'Game', 'Go']);
    expect(f.find((x) => x.label === 'JS/TS')!.count).toBe(3);
  });

  it('filters by ecosystem label', () => {
    expect(design.filter((p) => matches(p, 'Rust')).map((p) => p.name)).toEqual(['rust-svc', 'payments']);
    expect(design.filter((p) => matches(p, ALL))).toHaveLength(8);
    expect(design.filter((p) => matches(p, 'Game')).map((p) => p.name)).toEqual(['kart-game']);
  });

  it('works on the recorded real payload', () => {
    const real = MOCK_DATA.projects.projects;
    expect(filters(real).map((f) => f.label)).toEqual(['All', 'Rust', 'JS/TS']);
  });
});

describe('aggregation', () => {
  it('sorts by size, unknown sizes last', () => {
    const unknown: ProjectSummary = { ...byName('cli-tools'), name: 'zz', total_allocated: null, allocated: { s0: 0, s1: 0, s2: 0, s3: 0 } };
    const sorted = sortBySize([unknown, ...design]).map((p) => p.name);
    expect(sorted[0]).toBe('rust-svc');
    expect(sorted.at(-1)).toBe('zz');
    expect(projectSize(unknown)).toBeNull();
  });

  it('builds tier bars as shares of the project', () => {
    const bar = tierBar(byName('shop-api'));
    expect(bar.map((s) => s.tier)).toEqual(['S0', 'S1', 'S2', 'S3']);
    expect(bar[0]!.pct + bar[3]!.pct).toBeCloseTo(100, 0);
    // A real project: the bar leaves room for files that are not candidates.
    const api = MOCK_DATA.projects.projects.find((p) => p.name === 'api')!;
    const b = tierBar(api);
    expect(b[3]!.pct).toBeCloseTo((102400 / 122880) * 100, 3);
  });

  it('lists what is inside, never inventing a 0 size', () => {
    const api = MOCK_DATA.projects.projects.find((p) => p.name === 'api')!;
    expect(inside(api)).toEqual([
      { tier: 'S3', label: 'Protected', bytes: 102400 },
      { tier: null, label: 'Everything else', bytes: 20480 },
    ]);
  });

  it('formats paths and activity', () => {
    expect(shortPath('/Users/ada/code/api')).toBe('~/code/api');
    expect(shortPath('/home/ada/x')).toBe('~/x');
    expect(shortPath('C:\\Users\\ada\\dev')).toBe('~\\dev');
    expect(shortPath('/srv/x')).toBe('/srv/x');
    expect(lastActivity(1000, 1000 + 2 * 3600)).toBe('2 hours ago');
    expect(lastActivity(1000, 1000 + 3 * 86400)).toBe('3 days ago');
    expect(lastActivity(null, 1000)).toBe('unknown');
    expect(avatar({ ecosystems: ['rust'] })).toEqual({ initials: 'RS', tone: 'rust' });
  });
});

describe('per-project clean', () => {
  it('shrinks only regenerable bytes, with an ease-out', () => {
    const p = byName('shop-api');
    const half = shrink(p, 0.5);
    expect(half.allocated.s0).toBe(Math.round(p.allocated.s0 * 0.125));
    expect(half.allocated.s3).toBe(p.allocated.s3);
    const done = shrink(p, 1);
    expect(done.allocated.s0).toBe(0);
    expect(done.reclaimable.s0).toBe(0);
    expect(done.total_allocated).toBe(p.total_allocated! - p.allocated.s0);
    expect(done.badge).toBe('S3');
  });

  it('allows one clean at a time, only with regenerable bytes', () => {
    const s = idleClean();
    expect(canClean(s, byName('shop-api'))).toBe(true);
    expect(canClean(s, byName('payments'))).toBe(false);
    expect(canClean(s, null)).toBe(false);
    expect(canClean({ ...s, phase: 'cleaning', root: 'x' }, byName('shop-api'))).toBe(false);
  });

  it('the mock only animates; inside Tauri the action is not available yet', async () => {
    const m = createMockProjectActions({ durationMs: 0 });
    const seen: number[] = [];
    await expect(m.cleanRegenerable('/x', 5 * GiB, (f) => seen.push(f))).resolves.toEqual({ freed_bytes: 5 * GiB });
    expect(seen.at(-1)).toBe(1);
    await expect(createPendingProjectActions().cleanRegenerable('/x', 1)).rejects.toThrow(/Phase 2 executor/);
  });
});

async function mountAt(path: string, bridge: ScullBridge = createMockBridge()) {
  const router = createRouter({ history: createMemoryHistory(), routes });
  router.push(path);
  await router.isReady();
  const wrapper = mount(App, { global: { plugins: [router], provide: { [BRIDGE_KEY as symbol]: bridge } } });
  await new Promise((r) => setTimeout(r, 300));
  await flushPromises();
  return wrapper;
}

describe('projects screen', () => {
  it('lists the real projects from the bridge, sorted by size, with nothing to clean', async () => {
    const w = await mountAt('/projects');
    const rows = w.findAll('[data-testid="project-row"]');
    expect(rows).toHaveLength(MOCK_DATA.projects.projects.length);
    expect(rows[0]!.text()).toContain('app');
    expect(w.get('[data-testid="subtitle"]').text()).toContain('across ~/code');
    expect(w.get('[data-testid="clean"]').attributes('disabled')).toBeDefined();
    expect(w.get('[data-testid="clean"]').text()).toBe('Nothing regenerable to clean');
  });

  it('filters by ecosystem', async () => {
    const w = await mountAt('/projects');
    const js = w.findAll('[data-testid="filter"]').find((b) => b.text() === 'JS/TS')!;
    await js.trigger('click');
    expect(js.attributes('aria-pressed')).toBe('true');
    expect(w.findAll('[data-testid="project-row"]').map((r) => r.text())).toEqual([expect.stringContaining('web')]);
  });

  it('cleans a project in the design demo and shows the toast (06b)', async () => {
    const w = await mountAt('/projects?demo=design');
    await w.findAll('[data-testid="project-row"]')[1]!.trigger('click');
    expect(w.get('[data-testid="clean"]').text()).toBe('Clean regenerable · 12.8 GiB');
    await w.get('[data-testid="clean"]').trigger('click');
    await vi.waitFor(async () => {
      await flushPromises();
      expect(w.get('[data-testid="toast"]').text()).toContain('Cleaned regenerable files in shop-api');
    }, { timeout: 10000, interval: 25 });
    expect(w.get('[data-testid="toast"]').text()).toContain('+12.8 GiB');
    expect(w.get('[data-testid="detail-reclaim"]').text()).toBe('0 B');
    expect(w.get('[data-testid="detail-total"]').text()).toBe('8.5 GiB');
    expect(w.get('[data-testid="inside-cleaned"]').text()).toContain('cleaned');
    expect(w.findAll('[data-testid="row-reclaim"]')[1]!.text()).toBe('—');
    expect(w.get('[data-testid="clean"]').attributes('disabled')).toBeDefined();
  });
});
