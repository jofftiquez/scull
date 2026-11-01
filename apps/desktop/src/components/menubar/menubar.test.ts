// Menu bar widget (design 11 / 11b, P4-UI-01): the figures, the three states (idle, cleaning,
// done), the refusal inside Tauri, and the window commands. Nothing here deletes anything.
import { mount } from '@vue/test-utils';
import { afterEach, describe, expect, it, vi } from 'vitest';
import MenuBarWidget from '../../screens/MenuBar/MenuBarWidget.vue';
import { BRIDGE_KEY, type DesktopStatus, type ScullBridge } from '../../bridge';
import { createMockBridge, MOCK_DATA, MOCK_DRIVES } from '../../bridge/mock';
import { createTauriWindowCommands, NAVIGATE_EVENT, WINDOW_COMMANDS, type TauriTransport } from '../../bridge/tauri';
import { QUICK_CLEAN_UNAVAILABLE } from '../overview/quickClean';
import { designMenuBarBridge, wantsDesign } from './designScenario';
import {
  alertRows,
  checkedAgo,
  cleanFrame,
  daysLeft,
  MAIN_ROUTES,
  menuBarModel,
  pickReclaim,
  pickVolume,
  ring,
  RING_C,
  toolNames,
} from './model';
import { browserWidgetWindow, isMainRoute, WIDGET_WINDOW_KEY, type WidgetWindow } from './window';

const GiB = 1024 ** 3;
const NOW = 1_900_000_000;

function status(over: Partial<DesktopStatus> = {}): DesktopStatus {
  return { ...structuredClone(MOCK_DATA.status), now: NOW, ...over };
}

describe('menu bar model', () => {
  it('shows the home drive live, else the latest / sample', () => {
    expect(pickVolume([...MOCK_DRIVES], null)).toEqual({ label: 'Macintosh HD', free: Math.round(41.2 * GiB), total: Math.round(494 * GiB) });
    const s = status({
      volumes: [
        { volume_id: '/data', total_bytes: 100, free_bytes: 10, sampled_at: NOW },
        { volume_id: '/', total_bytes: 200, free_bytes: 50, sampled_at: NOW - 60 },
      ],
    });
    expect(pickVolume(null, s)).toEqual({ label: '/', free: 50, total: 200 });
    expect(pickVolume([], status({ volumes: [] }))).toBeNull();
  });

  it('prefers the ready-to-review plan, then the last scan’s safe bytes, never a made-up 0', () => {
    const plan = {
      plan_id: 'plan_01HZX',
      file: '/f.json',
      items: 3,
      reclaimable_bytes: 5 * GiB,
      created_at: NOW,
      expires_at: NOW + 1,
      includes_s1: false,
      review_command: 'scull clean --plan /f.json',
    };
    expect(pickReclaim(status({ review_plan: plan }))).toEqual({ bytes: 5 * GiB, source: 'plan', items: 3, reviewCommand: 'scull clean --plan /f.json' });
    const scanOnly = status({ last_scan: { ...MOCK_DATA.status.last_scan!, safe_reclaimable_bytes: GiB } });
    expect(pickReclaim(scanOnly)?.source).toBe('scan');
    expect(pickReclaim(status())).toBeNull();
    expect(pickReclaim(null)).toBeNull();
  });

  it('routes each alert to the screen that resolves it, with its tone', () => {
    const rows = alertRows([
      { kind: 'worktree_ttl_expired', severity: 'critical', key: 'a', subject: '/w', message: 'TTL', raised_at: NOW, paths: [] },
      { kind: 'budget_exceeded', severity: 'warning', key: 'b', subject: 'ml', message: 'budget', raised_at: NOW, paths: [] },
      { kind: 'stale', severity: 'info', key: 'c', subject: '/p', message: 'idle', raised_at: NOW, paths: [] },
      { kind: 'low_space', severity: 'critical', key: 'd', subject: '/', message: 'low', raised_at: NOW, paths: [] },
    ]);
    expect(rows.map((r) => [r.route, r.tone])).toEqual([
      ['/worktrees', 'critical'],
      ['/insights', 'warning'],
      ['/cleanup', 'info'],
      ['/insights', 'critical'],
    ]);
    for (const r of rows) expect(MAIN_ROUTES).toContain(r.route);
  });

  it('days left from the trend: none without one, at least 1', () => {
    expect(daysLeft(41.2 * GiB, 3.5 * GiB)).toBe(9);
    expect(daysLeft(56.1 * GiB, 3.5 * GiB)).toBe(13);
    expect(daysLeft(5 * GiB, 3.5 * GiB)).toBe(1);
    expect(daysLeft(41.2 * GiB, null)).toBeNull();
    expect(daysLeft(41.2 * GiB, 0)).toBeNull();
  });

  it('draws the ring like the design (used, then the reclaimable share at its end)', () => {
    const r = ring(41.2 * GiB, 494 * GiB, 38.6 * GiB);
    expect(r.usedPct).toBe(92);
    const usedLen = ((494 - 41.2) / 494) * RING_C;
    const recLen = (38.6 / 494) * RING_C;
    expect(r.usedDash).toBe(usedLen.toFixed(1));
    expect(r.recDash).toBe((recLen - 1).toFixed(1));
    expect(r.recOff).toBe((-(usedLen - recLen)).toFixed(1));
    expect(r.label).toBe('Disk 92% used, 41.2 GiB free of 494.0 GiB');
    // Reclaimable never exceeds what is used.
    expect(ring(10, 100, 1000).recOff).toBe('0.0');
  });

  it('Quick Clean frames: idle, cleaning (eased), done', () => {
    const T = 14.9 * GiB;
    expect(cleanFrame(T, 'idle', 0)).toEqual({ freed: 0, remaining: T, label: 'Quick Clean · 14.9 GiB', fill: 0 });
    const mid = cleanFrame(T, 'cleaning', 0.5);
    expect(mid.label).toBe('Cleaning… 88%');
    expect(mid.fill).toBe(88);
    expect(mid.freed + mid.remaining).toBeCloseTo(T);
    expect(cleanFrame(T, 'done', 1)).toMatchObject({ remaining: 0, label: '+14.9 GiB freed ✓', fill: 0 });
  });

  it('names the cache tools and how long ago the machine was checked', () => {
    const m = menuBarModel({ drives: null, status: status(), caches: MOCK_DATA.cachesReport, history: null });
    expect(toolNames(m.tools)).toBe('npm · re-downloadable');
    expect(m.perDay).toBeNull();
    expect(checkedAgo(status({ volumes: [{ volume_id: '/', total_bytes: 1, free_bytes: 1, sampled_at: NOW - 30 }] }))).toBe(30);
    expect(checkedAgo(status({ volumes: [], last_scan: null }))).toBeNull();
  });
});

describe('window actions', () => {
  it('browser: Open scull goes to the app page on the route; only known routes', async () => {
    const assign = vi.fn();
    await browserWidgetWindow({ assign }).openMain('/cleanup');
    expect(assign).toHaveBeenCalledWith('./index.html#/cleanup');
    expect(isMainRoute('/worktrees')).toBe(true);
    expect(isMainRoute('/onboarding')).toBe(false);
    expect(isMainRoute(42)).toBe(false);
  });

  it('tauri: open_main_window with the route, hide_menu_bar, and the navigate event', async () => {
    const calls: { cmd: string; args?: Record<string, unknown> }[] = [];
    let handler: ((p: unknown) => void) | null = null;
    const t: TauriTransport = {
      async invoke<T>(cmd: string, args?: Record<string, unknown>) {
        calls.push({ cmd, args });
        if (cmd === 'boom') throw { kind: 'usage', message: 'no' };
        return undefined as T;
      },
      async listen<T>(event: string, h: (p: T) => void) {
        expect(event).toBe(NAVIGATE_EVENT);
        handler = h as (p: unknown) => void;
        return () => (handler = null);
      },
    };
    const w = createTauriWindowCommands(t);
    await w.openMain('/insights');
    await w.hideWidget();
    expect(calls).toEqual([
      { cmd: WINDOW_COMMANDS.openMain, args: { route: '/insights' } },
      { cmd: WINDOW_COMMANDS.hideWidget, args: undefined },
    ]);
    const seen: string[] = [];
    const stop = await w.onNavigate((r) => seen.push(r));
    handler!('/caches');
    expect(seen).toEqual(['/caches']);
    stop();
    expect(handler).toBeNull();
  });
});

function fakeWindow(): WidgetWindow & { opened: string[]; hidden: number } {
  const w = {
    kind: 'browser' as const,
    opened: [] as string[],
    hidden: 0,
    async openMain(route: string) {
      w.opened.push(route);
    },
    async hide() {
      w.hidden += 1;
    },
  };
  return w;
}

function mountWidget(bridge: ScullBridge, win = fakeWindow()) {
  const wrapper = mount(MenuBarWidget, {
    attachTo: document.body,
    global: { provide: { [BRIDGE_KEY as symbol]: bridge, [WIDGET_WINDOW_KEY as symbol]: win } },
  });
  return { wrapper, win };
}

const text = (w: ReturnType<typeof mount>, id: string) => w.get(`[data-testid="${id}"]`).text();

describe('the widget', () => {
  afterEach(() => {
    vi.useRealTimers();
    document.body.innerHTML = '';
  });

  it('design data: idle → cleaning → done → idle, simulated on the mock bridge', async () => {
    const bridge = designMenuBarBridge(createMockBridge(), () => NOW);
    const { wrapper } = mountWidget(bridge);
    await vi.waitFor(() => expect(text(wrapper, 'free')).toBe('41.2 GiB'));
    await vi.waitFor(() => expect(text(wrapper, 'days-badge')).toBe('9 days left'));
    expect(text(wrapper, 'used-pct')).toBe('92%');
    expect(text(wrapper, 'reclaim-bytes')).toBe('38.6 GiB');
    expect(text(wrapper, 'cache-bytes')).toBe('14.9 GiB');
    expect(wrapper.findAll('[data-testid="alert"]')).toHaveLength(2);
    expect(text(wrapper, 'check')).toBe('Checked 4 min ago');
    expect(wrapper.get('main').attributes('data-phase')).toBe('idle');

    await wrapper.get('[data-testid="quick-clean"]').trigger('click');
    await vi.waitFor(() => expect(wrapper.get('main').attributes('data-phase')).toBe('cleaning'));
    expect(wrapper.get('[data-testid="quick-clean"]').attributes('disabled')).toBeDefined();
    await vi.waitFor(() => expect(wrapper.get('main').attributes('data-phase')).toBe('done'), { timeout: 15000 });
    expect(text(wrapper, 'quick-clean')).toBe('+14.9 GiB freed ✓');
    expect(text(wrapper, 'free')).toBe('56.1 GiB');
    expect(text(wrapper, 'days-badge')).toBe('13 days left');
    expect(text(wrapper, 'used-pct')).toBe('89%');
    expect(text(wrapper, 'simulated')).toBe('simulated · mock bridge, nothing deleted');

    await wrapper.get('[data-testid="quick-clean"]').trigger('click');
    expect(wrapper.get('main').attributes('data-phase')).toBe('idle');
    expect(text(wrapper, 'free')).toBe('41.2 GiB');
    wrapper.unmount();
  });

  it('inside Tauri Quick Clean is refused and nothing changes (the desktop never deletes)', async () => {
    const tauriLike: ScullBridge = { ...createMockBridge(), kind: 'tauri' };
    const caches = vi.spyOn(tauriLike, 'caches');
    const { wrapper } = mountWidget(tauriLike);
    await vi.waitFor(() => expect(wrapper.find('[data-testid="quick-clean"]').exists()).toBe(true));
    expect(caches).toHaveBeenCalled();
    await wrapper.get('[data-testid="quick-clean"]').trigger('click');
    await vi.waitFor(() => expect(wrapper.get('main').attributes('data-phase')).toBe('unavailable'));
    expect(text(wrapper, 'refused')).toBe(QUICK_CLEAN_UNAVAILABLE);
    expect(text(wrapper, 'free')).toBe('41.2 GiB');
    expect(text(wrapper, 'quick-clean')).toBe('OK');
    await wrapper.get('[data-testid="quick-clean"]').trigger('click');
    expect(wrapper.get('main').attributes('data-phase')).toBe('idle');
    wrapper.unmount();
  });

  it('every other control opens the main window; Escape hides the widget', async () => {
    const bridge = designMenuBarBridge(createMockBridge(), () => NOW);
    const { wrapper, win } = mountWidget(bridge);
    await vi.waitFor(() => expect(wrapper.findAll('[data-testid="alert"]')).toHaveLength(2));
    await wrapper.get('[data-testid="review"]').trigger('click');
    await wrapper.findAll('[data-testid="alert"]')[0]!.trigger('click');
    await wrapper.findAll('[data-testid="alert"]')[1]!.trigger('click');
    await wrapper.get('[data-testid="settings"]').trigger('click');
    await wrapper.get('[data-testid="open"]').trigger('click');
    expect(win.opened).toEqual(['/cleanup', '/worktrees', '/insights', '/settings', '/']);
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }));
    expect(win.hidden).toBe(1);
    wrapper.unmount();
  });

  it('Check now re-reads (live free space) and says so', async () => {
    const bridge = designMenuBarBridge(createMockBridge(), () => NOW);
    const drives = vi.spyOn(bridge, 'drives');
    const { wrapper } = mountWidget(bridge);
    await vi.waitFor(() => expect(text(wrapper, 'check')).toBe('Checked 4 min ago'));
    expect(drives).toHaveBeenCalledTimes(1);
    await wrapper.get('[data-testid="check"]').trigger('click');
    expect(text(wrapper, 'check')).toBe('Checking…');
    await vi.waitFor(() => expect(text(wrapper, 'check')).toBe('Checked just now'), { timeout: 5000 });
    expect(drives).toHaveBeenCalledTimes(2);
    wrapper.unmount();
  });

  it('recorded data: honest empty states (no plan, no alerts) and no Review button', async () => {
    const { wrapper } = mountWidget(createMockBridge());
    await vi.waitFor(() => expect(wrapper.get('main').attributes('aria-busy')).toBe('false'));
    expect(wrapper.get('[data-testid="reclaim"]').text()).toContain('Nothing safe to reclaim yet');
    expect(wrapper.find('[data-testid="review"]').exists()).toBe(false);
    expect(text(wrapper, 'no-alerts')).toBe('No alerts');
    wrapper.unmount();
  });

  it('a failing status call shows its error instead of figures', async () => {
    const bridge: ScullBridge = { ...createMockBridge(), status: () => Promise.reject({ kind: 'io', message: 'database locked' }), drives: () => Promise.resolve([]) };
    const { wrapper } = mountWidget(bridge);
    await vi.waitFor(() => expect(text(wrapper, 'error')).toBe('database locked'));
    expect(text(wrapper, 'free')).toBe('—');
    expect(wrapper.find('[data-testid="days-badge"]').exists()).toBe(false);
    wrapper.unmount();
  });
});

describe('design scenario', () => {
  it('is opt-in and never wraps the Tauri bridge', () => {
    expect(wantsDesign({ location: { search: '?demo=design', hash: '' } })).toBe(true);
    expect(wantsDesign({ location: { search: '', hash: '' } })).toBe(false);
    const tauriLike: ScullBridge = { ...createMockBridge(), kind: 'tauri' };
    expect(designMenuBarBridge(tauriLike)).toBe(tauriLike);
  });
});
