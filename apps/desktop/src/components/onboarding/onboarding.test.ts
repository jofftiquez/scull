import { flushPromises, mount } from '@vue/test-utils';
import { describe, expect, it } from 'vitest';
import { createMemoryHistory, createRouter } from 'vue-router';
import App from '../../App.vue';
import { BRIDGE_KEY, type ScanParams, type ScullBridge } from '../../bridge';
import { createMockBridge, MOCK_DATA } from '../../bridge/mock';
import { routes } from '../../navigation';
import { createPendingOnboardingBridge, MOCK_DETECTION } from './bridge';
import {
  answeredCount,
  broadRootConcern,
  canMove,
  checkRoot,
  defaultAnswers,
  displayPath,
  needsOnboarding,
  scanFill,
  scanRoots,
  summary,
  toProfile,
  toolchainDetail,
  totalEntries,
  type Detection,
} from './model';

const HOME = '/Users/ada';

describe('scan roots (CFG-04)', () => {
  it('flags filesystem roots, the home folder and its parents', () => {
    expect(broadRootConcern('/', HOME)).toBe('is a filesystem root');
    expect(broadRootConcern('C:\\', 'C:\\Users\\ada')).toBe('is a filesystem root');
    expect(broadRootConcern('D:', 'C:\\Users\\ada')).toBe('is a filesystem root');
    expect(broadRootConcern(HOME, HOME)).toBe('is your home folder');
    expect(broadRootConcern('/Users/ada/', HOME)).toBe('is your home folder');
    expect(broadRootConcern('c:\\users\\ADA', 'C:\\Users\\ada')).toBe('is your home folder');
    expect(broadRootConcern('/Users', HOME)).toBe('contains your home folder');
    expect(broadRootConcern('/Users/ada/code', HOME)).toBeNull();
    expect(broadRootConcern('/Users/adam', HOME)).toBeNull();
  });

  it('validates typed folders', () => {
    expect(checkRoot('~/Projects', HOME)).toEqual({ ok: true, path: '/Users/ada/Projects', broad: null });
    expect(checkRoot('~', HOME)).toEqual({ ok: true, path: HOME, broad: 'is your home folder' });
    expect(checkRoot('/srv/git/', HOME)).toEqual({ ok: true, path: '/srv/git', broad: null });
    expect(checkRoot('code', HOME).ok).toBe(false);
    expect(checkRoot('/Users/ada/../bob', HOME).ok).toBe(false);
    expect(checkRoot('  ', HOME).ok).toBe(false);
    expect(checkRoot('C:', 'C:\\Users\\ada')).toEqual({ ok: true, path: 'C:\\', broad: 'is a filesystem root' });
  });

  it('never ticks a broad folder by default, and drops unconfirmed broad roots', () => {
    const d: Detection = {
      ...MOCK_DETECTION,
      folders: [
        { path: HOME, repos: 4, suggested: true },
        { path: '/', repos: null, suggested: true },
        { path: '/Users/ada/code', repos: 32, suggested: true },
      ],
    };
    const a = defaultAnswers(d);
    expect(a.roots).toEqual(['/Users/ada/code']);
    a.roots = [HOME, '/Users/ada/code'];
    expect(scanRoots(a, HOME)).toEqual(['/Users/ada/code']);
    a.confirmedBroad = [HOME];
    expect(scanRoots(a, HOME)).toEqual([HOME, '/Users/ada/code']);
    expect(toProfile(a, d).roots).toEqual([HOME, '/Users/ada/code']);
  });

  it('shortens paths under home', () => {
    expect(displayPath('/Users/ada/code', HOME)).toBe('~/code');
    expect(displayPath(HOME, HOME)).toBe('~');
    expect(displayPath('/opt/x', HOME)).toBe('/opt/x');
    expect(displayPath('C:\\Users\\ada\\dev', 'C:\\Users\\ada')).toBe('~\\dev');
  });
});

describe('answers and summary (ONB-01…03)', () => {
  it('pre-fills from what was found', () => {
    const a = defaultAnswers(MOCK_DETECTION);
    expect(a.builds).toEqual(['web', 'rust', 'python', 'ml', 'game', 'infra']);
    expect(a.roots).toEqual(['/Users/ada/code', '/Users/ada/work']);
    expect(a.agents).toBe('daily');
    expect(a.connectAgents).toBe(true);
    expect(answeredCount(a, HOME)).toBe(5);
  });

  it('falls back to "other" and no agents when nothing was found', () => {
    const a = defaultAnswers({ home: HOME, toolchains: [], folders: [], ai_clients: [] });
    expect(a.builds).toEqual(['other']);
    expect(a.agents).toBe('none');
    expect(answeredCount(a, HOME)).toBe(4);
  });

  it('reads like the design', () => {
    const a = defaultAnswers(MOCK_DETECTION);
    a.builds = ['web', 'rust', 'ml'];
    expect(summary(a, MOCK_DETECTION)).toEqual([
      { k: 'Scan', v: '~/code, ~/work' },
      { k: 'Show first', v: 'Web / JS · Rust · ML / AI' },
      { k: 'Agents', v: 'Agent worktrees watched · Claude Code + Codex connected' },
      { k: 'Open on', v: 'Quick Clean + safest wins' },
      { k: 'Stale after', v: 'Idle 14 days' },
      { k: 'Checks', v: 'Every 6 h · alert at ~14 days to full' },
    ]);
    Object.assign(a, { roots: [], builds: [], agents: 'none', priority: 'understand', caution: 'cautious' });
    const s = Object.fromEntries(summary(a, MOCK_DETECTION).map((r) => [r.k, r.v]));
    expect(s).toMatchObject({
      Scan: 'Choose at least one folder',
      'Show first': 'Everything, by size',
      Agents: 'Off',
      'Open on': 'Storage map',
      'Stale after': 'Idle 30 days',
      Checks: 'Manual',
    });
  });

  it('caution never reaches safety settings (ONB-07)', () => {
    const a = defaultAnswers(MOCK_DETECTION);
    const keys = Object.keys(toProfile({ ...a, caution: 'expert' }, MOCK_DETECTION)).sort();
    expect(keys).toEqual(Object.keys(toProfile({ ...a, caution: 'cautious' }, MOCK_DETECTION)).sort());
    expect(keys.some((k) => /tier|protect|revalid|approv/.test(k))).toBe(false);
  });

  it('names toolchains by binary, else cache folder', () => {
    expect(toolchainDetail(MOCK_DETECTION.toolchains[1]!)).toBe('npm · pnpm · bun');
    expect(toolchainDetail({ id: 'g', label: 'G', build: 'game', binaries: ['/Applications/Unity Hub.app'], caches: [] })).toBe(
      'unity hub',
    );
    expect(toolchainDetail({ id: 'n', label: 'N', build: 'web', binaries: ['C:\\x\\npm.cmd'], caches: [] })).toBe('npm');
    expect(toolchainDetail({ id: 'm', label: 'M', build: 'ml', binaries: [], caches: ['/h/.cache/huggingface'] })).toBe(
      'huggingface',
    );
  });
});

describe('first scan state machine', () => {
  it('only moves along allowed transitions', () => {
    expect(canMove('idle', 'running')).toBe(true);
    expect(canMove('idle', 'done')).toBe(false);
    expect(canMove('running', 'done')).toBe(true);
    expect(canMove('done', 'running')).toBe(false);
    expect(canMove('failed', 'running')).toBe(true);
    expect(canMove('cancelled', 'running')).toBe(true);
  });

  it('eases the bar to 90% while running and fills on completion', () => {
    expect(scanFill('idle', 5000)).toBe(0);
    expect(scanFill('running', 0)).toBe(0);
    expect(scanFill('running', 1600)).toBeCloseTo(0.675);
    expect(scanFill('running', 99999)).toBeCloseTo(0.9);
    expect(scanFill('done', 0)).toBe(1);
    expect(totalEntries({ '/a': 10, '/b': 5 })).toBe(15);
  });

  it('starts on onboarding only before the first scan', () => {
    expect(needsOnboarding(null)).toBe(false);
    expect(needsOnboarding({ state: 'no_scan' })).toBe(true);
    expect(needsOnboarding({ state: 'no_database' })).toBe(true);
    expect(needsOnboarding({ state: 'ok' })).toBe(false);
  });
});

describe('pending onboarding bridge (inside Tauri)', () => {
  it('derives detection from read-only calls and never claims to save', async () => {
    const b = createPendingOnboardingBridge(createMockBridge());
    const d = await b.detect();
    expect(d.home).toBe(HOME);
    expect(d.folders.map((f) => f.path)).toEqual(MOCK_DATA.projects.roots);
    expect(d.toolchains.map((t) => t.id)).toEqual(expect.arrayContaining(['node', 'rust']));
    const r = await b.saveProfile(toProfile(defaultAnswers(d), d));
    expect(r.saved).toBe(false);
  });
});

async function mountApp(path: string, bridge: ScullBridge) {
  const router = createRouter({ history: createMemoryHistory(), routes });
  router.push(path);
  await router.isReady();
  const wrapper = mount(App, { global: { plugins: [router], provide: { [BRIDGE_KEY as symbol]: bridge } }, attachTo: document.body });
  await flushPromises();
  return { wrapper, router };
}

describe('onboarding screen', () => {
  it('opens on first run, full window, and scans only the chosen roots', async () => {
    const calls: ScanParams[] = [];
    const base = createMockBridge();
    const bridge: ScullBridge = {
      ...base,
      status: async () => ({ ...MOCK_DATA.status, state: 'no_scan', last_scan: null }),
      scan: async (p, o) => {
        calls.push(p);
        return base.scan(p, o);
      },
    };
    const { wrapper, router } = await mountApp('/', bridge);
    await new Promise((r) => setTimeout(r, 600));
    await flushPromises();
    expect(router.currentRoute.value.path).toBe('/onboarding');
    await new Promise((r) => setTimeout(r, 200));
    await flushPromises();
    expect(wrapper.find('[data-testid="sidebar"]').exists()).toBe(false);
    expect(wrapper.get('[data-testid="answered"]').text()).toBe('5 of 5 answered');
    expect(wrapper.get('[data-testid="found"]').text()).toContain('494.0 GiB · 41.2 GiB free');

    // Ticking the home folder asks first (CFG-04); declining leaves it out.
    await wrapper.get('button.btn-outline').trigger('click');
    await wrapper.get('#add-folder').setValue('~');
    await wrapper.get('form').trigger('submit');
    expect(wrapper.get('[data-testid="broad-confirm"]').text()).toContain('is your home folder');
    await wrapper.get('[data-testid="broad-confirm"] .btn-outline').trigger('click');
    expect(wrapper.find('[data-testid="broad-confirm"]').exists()).toBe(false);

    await wrapper.get('[data-testid="start-scan"]').trigger('click');
    await new Promise((r) => setTimeout(r, 1800));
    await flushPromises();
    expect(calls).toEqual([{ roots: ['/Users/ada/code', '/Users/ada/work'], allow_broad_roots: false }]);
    expect(wrapper.get('[data-testid="scan"]').text()).toContain('Scan complete');
    expect(wrapper.get('[data-testid="found-chips"]').text()).toContain(`${MOCK_DATA.scanReport.projects.length} projects`);
    expect(wrapper.find('[data-testid="open-overview"]').exists()).toBe(true);
    wrapper.unmount();
  });

  it('disables the scan until a folder is chosen', async () => {
    const { wrapper } = await mountApp('/onboarding', createMockBridge());
    await new Promise((r) => setTimeout(r, 200));
    await flushPromises();
    for (const box of wrapper.findAll('[data-testid="folder"] input')) await box.setValue(false);
    expect(wrapper.get('[data-testid="start-scan"]').attributes('disabled')).toBeDefined();
    expect(wrapper.get('[data-testid="summary"]').text()).toContain('Choose at least one folder');
    expect(wrapper.get('[data-testid="answered"]').text()).toBe('4 of 5 answered');
    wrapper.unmount();
  });
});
