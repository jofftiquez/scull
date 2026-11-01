import { flushPromises, mount } from '@vue/test-utils';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { createMemoryHistory, createRouter } from 'vue-router';
import App from '../../App.vue';
import { BRIDGE_KEY, type Candidate, type Drive, type ProjectSummary, type ScanReport, type ScullBridge, type Tier } from '../../bridge';
import { createMockBridge, MOCK_DATA, MOCK_DRIVES } from '../../bridge/mock';
import { routes } from '../../navigation';
import { core, resetCore } from '../../state/core';
import { createMockExplorerActions, createPendingExplorerActions, explorerActionsFor } from './actions';
import { designExplorerReport } from './designScenario';
import {
  buildExplorer,
  cleanupCandidates,
  detailAction,
  detailKind,
  detailNote,
  displayPath,
  homeOf,
  largestInside,
  machineName,
  protectionReasons,
  relativeSegments,
  REST_NAME,
  REST_NOTE,
  splitPath,
  tierShares,
  trailOf,
  unscannedWhy,
  visibleChildren,
  type ExNode,
} from './model';
import { layoutTreemap, squarify, worstRatio, type Rect } from './treemap';

const GiB = 1024 ** 3;
const gib = (b: number) => Math.round((b / GiB) * 10) / 10;

/* ------------------------------------------------------------------ treemap */

const area = (r: Rect) => r.w * r.h;
const overlap = (a: Rect, b: Rect) => Math.max(0, Math.min(a.x + a.w, b.x + b.w) - Math.max(a.x, b.x)) * Math.max(0, Math.min(a.y + a.h, b.y + b.h) - Math.max(a.y, b.y));

describe('squarify', () => {
  const box = { x: 0, y: 0, w: 740, h: 566 };

  it('gives every item an area proportional to its value, inside the box, without overlaps', () => {
    const values = [27.1, 24.6, 21.3, 19.2, 12.5, 3, 1, 0.5];
    const tiles = squarify(values, (v) => v, box);
    const total = values.reduce((a, b) => a + b, 0);
    expect(tiles).toHaveLength(values.length);
    for (const t of tiles) {
      expect(area(t) / area(box)).toBeCloseTo(t.value / total, 6);
      expect(t.x).toBeGreaterThanOrEqual(-1e-9);
      expect(t.y).toBeGreaterThanOrEqual(-1e-9);
      expect(t.x + t.w).toBeLessThanOrEqual(box.w + 1e-6);
      expect(t.y + t.h).toBeLessThanOrEqual(box.h + 1e-6);
    }
    for (let i = 0; i < tiles.length; i++) for (let j = i + 1; j < tiles.length; j++) expect(overlap(tiles[i]!, tiles[j]!)).toBeLessThan(1e-6);
    expect(tiles.reduce((a, t) => a + area(t), 0)).toBeCloseTo(area(box), 3);
  });

  it('puts the design drives side by side, the bigger one first (design 07)', () => {
    const tiles = squarify([{ n: 'Macintosh HD', v: 186.0 }, { n: 'Dev SSD', v: 262.8 }], (d) => d.v, box);
    expect(tiles.map((t) => t.item.n)).toEqual(['Dev SSD', 'Macintosh HD']);
    expect(tiles[0]!.h).toBeCloseTo(box.h);
    expect(tiles[0]!.w).toBeCloseTo((262.8 / 448.8) * box.w, 6);
    expect(tiles[1]!.x).toBeCloseTo(tiles[0]!.w);
  });

  it('keeps aspect ratios low (squarified, not slices)', () => {
    const values = Array.from({ length: 30 }, (_, i) => 30 - i);
    const tiles = squarify(values, (v) => v, box);
    const worst = Math.max(...tiles.map((t) => Math.max(t.w / t.h, t.h / t.w)));
    expect(worst).toBeLessThan(4);
  });

  it('drops zero, negative and non-finite values, and handles empty input', () => {
    expect(squarify([0, -1, NaN, Infinity], (v) => v, box)).toEqual([]);
    expect(squarify([], (v: number) => v, box)).toEqual([]);
    expect(squarify([1, 2], (v) => v, { x: 0, y: 0, w: 0, h: 10 })).toEqual([]);
    expect(squarify([5, 0], (v) => v, box)).toHaveLength(1);
  });

  it('keeps input order for ties', () => {
    const tiles = squarify(['a', 'b', 'c'], () => 1, box);
    expect(tiles.map((t) => t.item)).toEqual(['a', 'b', 'c']);
  });

  it('worstRatio is 1 for a square and grows as rows get thin', () => {
    expect(worstRatio([100], 10)).toBeCloseTo(1);
    expect(worstRatio([10, 10], 10)).toBeGreaterThan(1);
    expect(worstRatio([], 10)).toBe(Infinity);
  });
});

describe('layoutTreemap', () => {
  it('leaves exactly `gap` between tiles and none at the edges', () => {
    const tiles = layoutTreemap([262.8, 186.0], (v) => v, 734, 560, 6);
    const [a, b] = [tiles[0]!, tiles[1]!];
    expect(a.x).toBeCloseTo(0);
    expect(a.y).toBeCloseTo(0);
    expect(a.h).toBeCloseTo(560);
    expect(b.x - (a.x + a.w)).toBeCloseTo(6);
    expect(b.x + b.w).toBeCloseTo(734);
  });
});

/* ------------------------------------------------------------------ model */

function cand(path: string, tier: Tier, bytes: number, reason = 'why'): Candidate {
  return {
    action: { type: 'none' },
    artifact: '',
    category: 'project_artifact',
    evidence: [],
    flags: {},
    id: `id:${path}`,
    identity: { file: path, volume: 1 },
    path,
    project: null,
    reasons: [{ code: 'regenerable', message: reason, tier }],
    regeneration: { cost: { type: 'unknown' }, instructions: '' },
    rule: { id: 't', version: 1 },
    sizes: { allocated: bytes, logical: bytes, reclaimable: { bytes, confidence: 'high', shared_bytes: 0 } },
    tier,
  };
}

function proj(root: string, total: number | null): ProjectSummary {
  const z = { s0: 0, s1: 0, s2: 0, s3: 0 };
  return { allocated: z, candidates: 0, ecosystems: [], markers: [], name: root, reclaimable: z, root, total_allocated: total };
}

function report(candidates: Candidate[], projects: ProjectSummary[] = []): ScanReport {
  return { ...MOCK_DATA.scanReport, candidates, projects };
}

const find = (n: ExNode, name: string): ExNode => {
  const c = n.children.find((x) => x.name === name);
  if (!c) throw new Error(`no ${name} in ${n.name}: ${n.children.map((x) => x.name).join(', ')}`);
  return c;
};

describe('paths', () => {
  it('splits and relates POSIX and Windows paths', () => {
    expect(splitPath('/Users/ada/code')).toEqual(['Users', 'ada', 'code']);
    expect(splitPath('C:\\Users\\ada/code')).toEqual(['C:', 'Users', 'ada', 'code']);
    expect(relativeSegments('/Volumes/Dev SSD/x', '/Volumes/Dev SSD')).toEqual(['x']);
    expect(relativeSegments('/Volumes/Dev SSDx', '/Volumes/Dev SSD')).toBeNull();
    expect(relativeSegments('/a', '/')).toEqual(['a']);
    expect(relativeSegments('c:\\users\\ADA', 'C:\\')).toEqual(['users', 'ADA']);
    expect(relativeSegments('C:\\x', '/')).toBeNull();
    expect(homeOf('/Users/ada/code')).toBe('/Users/ada');
    expect(homeOf('/home/ada')).toBe('/home/ada');
    expect(homeOf('C:\\Users\\ada\\src')).toBe('C:\\Users\\ada');
    expect(homeOf('/opt/x')).toBeNull();
  });
});

describe('buildExplorer: the design machine', () => {
  const ex = buildExplorer(designExplorerReport(), MOCK_DRIVES);

  it('roots at This Mac with the two scanned drives, sized by what was scanned (design 07)', () => {
    expect(ex.root.name).toBe('This Mac');
    expect(ex.root.children.map((d) => [d.name, d.kind, gib(d.bytes)])).toEqual([
      ['Dev SSD', 'drive', 262.8],
      ['Macintosh HD', 'drive', 186.0],
    ]);
    expect(gib(ex.root.bytes)).toBe(448.8);
    const t = ex.root.tiers;
    expect([t.S0, t.S1, t.S2, t.S3].map(gib)).toEqual([82.9, 75.4, 127.2, 163.3]);
  });

  it('lists the backup drive apart: never scanned (D-22)', () => {
    expect(ex.unscanned.map((u) => [u.drive.label, u.why])).toEqual([['Backup (Time Machine)', 'never scanned']]);
  });

  it('drills into folders home-relative on the home drive (design 07b)', () => {
    const hd = find(ex.root, 'Macintosh HD');
    expect(hd.children.map((c) => c.name)).toEqual(['code', '.cache', 'Docker build cache', 'work/payments']);
    const code = find(hd, 'code');
    expect(code.path).toBe('/Users/ada/code');
    expect(code.children.map((c) => [c.name, gib(c.bytes)])).toEqual([
      ['.worktrees', 27.1],
      ['rust-svc', 24.6],
      ['shop-api', 21.3],
      ['ml-lab', 19.2],
      ['kart-game', 12.5],
    ]);
    expect([code.tiers.S0, code.tiers.S1, code.tiers.S2, code.tiers.S3].map(gib)).toEqual([34.7, 8.2, 19.8, 42.0]);
  });

  it("adds a project's source as one protected tile, and a source-only project becomes an item", () => {
    const code = find(find(ex.root, 'Macintosh HD'), 'code');
    const rust = find(code, 'rust-svc');
    expect(rust.project).toBe(true);
    expect(rust.children.map((c) => [c.name, c.tier, gib(c.bytes)])).toEqual([
      ['target', 'S0', 15.2],
      [REST_NAME, 'S3', 9.4],
    ]);
    const payments = find(find(ex.root, 'Macintosh HD'), 'work/payments');
    expect(payments).toMatchObject({ kind: 'item', tier: 'S3', path: '/Users/ada/work/payments' });
  });

  it('walks a trail and builds display paths like the design', () => {
    const hd = find(ex.root, 'Macintosh HD');
    const trail = trailOf(ex.root, [hd.id, find(hd, 'code').id, 'nope']);
    expect(trail.map((n) => n.name)).toEqual(['This Mac', 'Macintosh HD', 'code']);
    expect(displayPath(trail)).toBe('Macintosh HD/code');
    expect(displayPath([ex.root])).toBe('This Mac');
    expect(largestInside(ex.root, 'This Mac').map((l) => [l.path, gib(l.bytes)])).toEqual([
      ['This Mac/Dev SSD/datasets', 88.0],
      ['This Mac/Dev SSD/vm-images/win11.vhdx', 63.5],
      ['This Mac/Dev SSD/vm-images/ubuntu-24.qcow2', 41.0],
      ['This Mac/Macintosh HD/.cache/huggingface', 31.2],
      ['This Mac/Dev SSD/projects-archive/node_modules ×23', 29.6],
    ]);
  });

  it('picks the aside action, kind and note', () => {
    const hd = find(ex.root, 'Macintosh HD');
    const code = find(hd, 'code');
    expect(detailAction(ex.root)).toBe('why');
    expect(detailAction(find(hd, '.cache'))).toBe('add');
    expect(detailAction(find(code, 'ml-lab'))).toBe('review');
    expect(detailNote(code)).toBe('34.7 GiB in here rebuilds itself.');
    expect(detailNote(find(find(code, 'ml-lab'), '.venv'))).toBe('No lockfile.');
    expect(detailKind(code, false)).toBe('This folder');
    expect(detailKind(code, true)).toBe('Folder');
    expect(detailKind(find(find(code, 'rust-svc'), 'target'), true)).toBe('Item · Regenerable');
  });

  it('explains protection and lists what a cleanup could take', () => {
    const code = find(find(ex.root, 'Macintosh HD'), 'code');
    // The projects' source (27.7 GiB) outweighs the unpushed worktrees (14.3 GiB).
    expect(protectionReasons(code)).toEqual([REST_NOTE, 'Commits that exist only on this Mac.']);
    expect(protectionReasons(find(code, 'rust-svc'))).toEqual(['Source code, Git history and other project files. scull never touches them.']);
    expect(cleanupCandidates(code).map((c) => c.path.split('/').at(-1))).toEqual(['target', 'node_modules', 'Library', 'worktree artifacts', 'dist']);
    const s = tierShares(code.tiers);
    expect(s.S0 + s.S1 + s.S2 + s.S3).toBeCloseTo(100);
  });
});

describe('buildExplorer: rules', () => {
  const home: Drive = { mount_point: '/', label: 'Disk', kind: 'local', scan_policy: 'scan', holds_home: true, total_bytes: 100, free_bytes: 50 };

  it('never counts a candidate inside another candidate, or a project inside a candidate, twice', () => {
    const ex = buildExplorer(
      report(
        [cand('/Users/a/code/wt/node_modules', 'S0', 10), cand('/Users/a/code/wt', 'S2', 50), cand('/Users/a/code/x/target', 'S0', 5)],
        [proj('/Users/a/code/wt', 70), proj('/Users/a/code/x', 8)],
      ),
      [home],
    );
    const code = find(find(ex.root, 'Disk'), 'code');
    expect(find(code, 'wt')).toMatchObject({ kind: 'item', bytes: 50, tier: 'S2' });
    expect(find(code, 'x').children.map((c) => [c.name, c.bytes])).toEqual([
      ['target', 5],
      [REST_NAME, 3],
    ]);
    expect(code.bytes).toBe(58);
  });

  it('works on the recorded real scan: totals add up, nothing double counted', () => {
    const r = MOCK_DATA.scanReport;
    const ex = buildExplorer(r, MOCK_DRIVES);
    expect(ex.root.children.map((d) => d.name)).toEqual(['Macintosh HD']);
    const projectsTotal = r.projects.reduce((a, p) => a + (p.total_allocated ?? 0), 0);
    const outside = r.candidates.filter((c) => !r.projects.some((p) => c.path.startsWith(`${p.root}/`) || c.path === p.root));
    expect(ex.root.bytes).toBe(projectsTotal + outside.reduce((a, c) => a + c.sizes.allocated, 0));
    expect(ex.unscanned.map((u) => u.why)).toEqual(['nothing scanned here yet', 'never scanned']);
  });

  it('puts folders outside the home folder apart on the home drive, and unknown places under Other locations', () => {
    const ex = buildExplorer(report([cand('/Users/a/code/t', 'S0', 4), cand('/opt/tool/cache', 'S1', 2), cand('D:\\x\\y', 'S1', 1)]), [home]);
    const disk = find(ex.root, 'Disk');
    expect(disk.children.map((c) => c.name).sort()).toEqual(['/opt/tool/cache', 'code/t']);
    expect(find(disk, '/opt/tool/cache').path).toBe('/opt/tool/cache');
    expect(find(ex.root, 'Other locations').children.map((c) => [c.name, c.path])).toEqual([['x/y', 'D:\\x\\y']]);
  });

  it('maps Windows paths to their drive letter, case-insensitively', () => {
    const c: Drive = { mount_point: 'C:\\', label: 'Windows (C:)', kind: 'local', scan_policy: 'scan', holds_home: true };
    const d: Drive = { mount_point: 'D:\\', label: 'Data (D:)', kind: 'removable', scan_policy: 'scan', holds_home: false };
    const ex = buildExplorer(
      report([cand('C:\\Users\\ada\\code\\api\\target', 'S0', 3), cand('c:\\users\\ada\\code\\web\\node_modules', 'S0', 2), cand('D:\\src\\big.vhdx', 'S2', 9)]),
      [c, d],
      machineName('windows'),
    );
    expect(ex.root.name).toBe('This PC');
    expect(ex.root.children.map((n) => [n.name, n.bytes])).toEqual([
      ['Data (D:)', 9],
      ['Windows (C:)', 5],
    ]);
    const code = find(find(ex.root, 'Windows (C:)'), 'code');
    expect(code.children.map((n) => n.name).sort()).toEqual(['api/target', 'web/node_modules']);
    expect(find(code, 'api/target').path).toBe('C:\\Users\\ada\\code\\api\\target');
  });

  it('says why a drive is not in the map (D-22)', () => {
    const nas: Drive = { mount_point: '/Volumes/NAS', label: 'NAS', kind: 'network', scan_policy: 'off_by_default', holds_home: false };
    expect(unscannedWhy(nas)).toBe('network drive · off by default');
    expect(unscannedWhy(MOCK_DRIVES[2]!)).toBe('never scanned');
    expect(unscannedWhy(MOCK_DRIVES[1]!)).toBe('nothing scanned here yet');
    const ex = buildExplorer(null, [home, nas]);
    expect(ex.root.children).toEqual([]);
    expect(ex.unscanned.map((u) => u.drive.label)).toEqual(['Disk', 'NAS']);
  });

  it('groups tiles too small to draw unless hidden folders are shown', () => {
    const kids = [cand('/Users/a/p/big', 'S0', 1000), cand('/Users/a/p/s1', 'S1', 5), cand('/Users/a/p/s2', 'S1', 4), cand('/Users/a/p/mid', 'S2', 200)];
    const p = find(find(buildExplorer(report(kids), [home]).root, 'Disk'), 'p');
    const grouped = visibleChildren(p, false);
    expect(grouped.map((n) => [n.name, n.kind, n.bytes])).toEqual([
      ['big', 'item', 1000],
      ['mid', 'item', 200],
      ['2 smaller items', 'group', 9],
    ]);
    expect(visibleChildren(p, true)).toHaveLength(4);
    expect(largestInside(grouped[2]!, 'Disk/p').map((l) => l.path)).toEqual(['Disk/p/s1', 'Disk/p/s2']);
  });
});

/* ------------------------------------------------------------------ actions */

describe('explorer actions', () => {
  it('mock records, pending rejects with a BridgeError (the desktop never deletes)', async () => {
    const m = createMockExplorerActions();
    expect(await m.addToCleanup(['a', 'b'])).toEqual({ queued: 2 });
    await m.reveal('/x');
    expect(m.calls).toEqual(['add a,b', 'reveal /x']);
    const p = createPendingExplorerActions();
    await expect(p.addToCleanup(['a'])).rejects.toMatchObject({ name: 'BridgeError' });
    await expect(p.reveal('/x')).rejects.toMatchObject({ name: 'BridgeError' });
    expect(explorerActionsFor(createMockBridge()).kind).toBe('mock');
    expect(explorerActionsFor({ ...createMockBridge(), kind: 'tauri' }).kind).toBe('pending');
  });
});

/* ------------------------------------------------------------------ screen */

async function mountAt(path: string, bridge: ScullBridge = createMockBridge()) {
  const router = createRouter({ history: createMemoryHistory(), routes });
  router.push(path);
  await router.isReady();
  const wrapper = mount(App, { attachTo: document.body, global: { plugins: [router], provide: { [BRIDGE_KEY as symbol]: bridge } } });
  await flushPromises();
  return { wrapper, router };
}

const tiles = (w: ReturnType<typeof mount>) => w.findAll('[data-testid="tile"]');
const tile = (w: ReturnType<typeof mount>, id: string) => w.find(`[data-testid="tile"][data-id="${id}"]`);

describe('explorer screen', () => {
  beforeEach(() => resetCore());
  afterEach(() => {
    document.body.innerHTML = '';
    resetCore();
  });

  it('shows the drives root, drills down with breadcrumbs and goes back up (design 07 / 07b)', async () => {
    const { wrapper: w } = await mountAt('/explorer?demo=design');
    await vi.waitFor(() => expect(tiles(w)).toHaveLength(2));
    expect(w.get('[data-testid="subtitle"]').text()).toContain('2 drives scanned');
    expect(tiles(w).map((t) => t.attributes('aria-label'))).toEqual(['Open drive Dev SSD · external, 262.8 GiB', 'Open drive Macintosh HD · internal, 186.0 GiB']);
    expect(w.get('[data-testid="up"]').attributes('disabled')).toBeDefined();
    expect(w.get('[data-testid="detail-name"]').text()).toBe('This Mac · 2 drives');
    expect(w.get('[data-testid="unscanned"]').text()).toContain('Backup (Time Machine) · never scanned');

    await tile(w, 'drive:/').trigger('click');
    await tile(w, '/Users/ada/code').trigger('keydown', { key: 'Enter' });
    await vi.waitFor(() => expect(tiles(w)).toHaveLength(5));
    expect(w.findAll('[data-testid="crumb"]').map((c) => c.text())).toEqual(['This Mac', 'Macintosh HD', 'code']);
    expect(w.get('[data-testid="here"]').text()).toBe('104.7 GiB · 5 items');
    expect(w.get('[data-testid="detail-path"]').text()).toBe('Macintosh HD/code');
    expect(w.findAll('[data-testid="largest"]')).toHaveLength(5);
    expect(w.find('[data-testid="unscanned"]').exists()).toBe(false);

    await w.findAll('[data-testid="crumb"]')[1]!.trigger('click');
    await vi.waitFor(() => expect(tiles(w)).toHaveLength(4));
    await w.get('[data-testid="up"]').trigger('click');
    await vi.waitFor(() => expect(tiles(w)).toHaveLength(2));
    w.unmount();
  });

  it('inspects an item: ring, aside, why protected, show in Finder', async () => {
    const { wrapper: w } = await mountAt('/explorer?demo=design&at=Macintosh HD/code/rust-svc');
    await vi.waitFor(() => expect(tiles(w)).toHaveLength(2));
    const rest = tile(w, '/Users/ada/code/rust-svc#rest');
    expect(rest.attributes('aria-pressed')).toBe('false');
    await rest.trigger('click');
    await vi.waitFor(() => expect(w.find('[data-testid="selected-ring"]').exists()).toBe(true));
    expect(tile(w, '/Users/ada/code/rust-svc#rest').attributes('aria-pressed')).toBe('true');
    expect(w.get('[data-testid="detail-kind"]').text()).toBe('Item · Protected');
    expect(w.get('[data-testid="detail-action"]').text()).toBe('Why protected?');
    await w.get('[data-testid="detail-action"]').trigger('click');
    await vi.waitFor(() => expect(w.find('[data-testid="reasons"]').text()).toContain('scull never touches them'));
    await w.get('[data-testid="detail-reveal"]').trigger('click');
    await flushPromises();
    expect(w.find('[data-testid="detail-message"]').exists()).toBe(false);

    // Clicking again deselects; the aside goes back to the folder.
    await tile(w, '/Users/ada/code/rust-svc#rest').trigger('click');
    await vi.waitFor(() => expect(w.get('[data-testid="detail-kind"]').text()).toBe('This folder'));
    w.unmount();
  });

  it('adds what can go to the cleanup through the action stub', async () => {
    const { wrapper: w } = await mountAt('/explorer?demo=design&at=Macintosh HD/.cache');
    await vi.waitFor(() => expect(w.get('[data-testid="detail-action"]').text()).toBe('Add to cleanup'));
    await w.get('[data-testid="detail-action"]').trigger('click');
    await vi.waitFor(() => expect(w.get('[data-testid="detail-message"]').text()).toBe('6 items added to Cleanup Review.'));
    w.unmount();
  });

  it('inside Tauri the pending stub explains instead of acting', async () => {
    const bridge = { ...createMockBridge(), kind: 'tauri' as const };
    core.report = designExplorerReport();
    const { wrapper: w } = await mountAt('/explorer?at=Macintosh HD/.cache', bridge);
    await vi.waitFor(() => expect(w.get('[data-testid="detail-action"]').text()).toBe('Add to cleanup'));
    await w.get('[data-testid="detail-action"]').trigger('click');
    await vi.waitFor(() => expect(w.get('[data-testid="detail-message"]').text()).toContain('Phase 2 plan bridge'));
    w.unmount();
  });

  it('toggles grouping of small tiles with "Show hidden folders"', async () => {
    const { wrapper: w } = await mountAt('/explorer?demo=design&at=Macintosh HD/code/shop-api');
    await vi.waitFor(() => expect(tiles(w).map((t) => t.attributes('data-id'))).toContain('/Users/ada/code/shop-api/dist'));
    // dist (0.4 of 21.3 GiB) is under 1% but alone, so it is not grouped.
    const btn = w.get('[data-testid="show-hidden"]');
    expect(btn.attributes('aria-pressed')).toBe('false');
    await btn.trigger('click');
    expect(btn.attributes('aria-pressed')).toBe('true');
    w.unmount();
  });

  it('draws the recorded real scan from core.report once the scan ends', async () => {
    const { wrapper: w } = await mountAt('/explorer');
    await vi.waitFor(() => expect(tiles(w)).toHaveLength(1), { timeout: 5000 });
    expect(tiles(w)[0]!.attributes('aria-label')).toMatch(/^Open drive Macintosh HD · internal/);
    expect(w.get('[data-testid="unscanned"]').text()).toContain('Dev SSD · nothing scanned here yet');
    w.unmount();
  });

  it('shows the empty state before any scan on a first run', async () => {
    const base = createMockBridge();
    const bridge: ScullBridge = { ...base, status: async () => ({ ...(await base.status()), state: 'no_scan' }) };
    const { wrapper: w } = await mountAt('/explorer', bridge);
    await vi.waitFor(() => expect(w.find('[data-testid="empty"]').text()).toContain('Nothing scanned yet'), {
      timeout: 5000,
    });
    expect(w.find('[data-testid="detail"]').exists()).toBe(false);
    w.unmount();
  });
});
