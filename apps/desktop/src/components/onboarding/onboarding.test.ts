import { flushPromises, mount } from '@vue/test-utils';
import { describe, expect, it, vi } from 'vitest';
import { createMemoryHistory, createRouter } from 'vue-router';
import App from '../../App.vue';
import { BRIDGE_KEY, type ScullBridge } from '../../bridge';
import { createMockBridge, MOCK_DATA } from '../../bridge/mock';
import { routes } from '../../navigation';
import { createPendingOnboardingBridge, MOCK_DETECTION } from './bridge';
import {
  broadRootConcern,
  canMove,
  checkRoot,
  defaultAnswers,
  displayPath,
  needsOnboarding,
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
    expect(a.connectAgents).toBe(false);
  });

  it('falls back to "other" and no agents when nothing was found', () => {
    const a = defaultAnswers({ home: HOME, toolchains: [], folders: [], ai_clients: [] });
    expect(a.builds).toEqual(['other']);
    expect(a.agents).toBe('none');
  });

  it('reviews answers without claiming unimplemented automation or connections', () => {
    const a = defaultAnswers(MOCK_DETECTION);
    a.builds = ['web', 'rust', 'ml'];
    expect(summary(a, MOCK_DETECTION).map(({ k, v }) => ({ k, v }))).toEqual([
      { k: 'Your work', v: 'Web / JS · Rust · ML / AI' },
      { k: 'Folders to scan', v: '~/code, ~/work' },
      { k: 'Coding agents', v: 'Daily' },
      { k: 'Your goal', v: 'Free space now' },
      { k: 'Explanation preference', v: 'Balanced' },
    ]);
    a.roots = [];
    expect(summary(a, MOCK_DETECTION)[1]!.v).toBe('Choose at least one folder');
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

  it('sums real per-root entry counters', () => {
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
    const b = createPendingOnboardingBridge(createMockBridge(), { context: async () => ({ home: HOME }), pickFolders: async () => null });
    const d = await b.detect();
    expect((await b.context()).home).toBe(HOME);
    expect(d.home).toBe('');
    expect(d.folders.map((f) => f.path)).toEqual(MOCK_DATA.projects.roots);
    expect(d.toolchains.map((t) => t.id)).toEqual(expect.arrayContaining(['node', 'rust']));
    expect(await b.pickFolders()).toBeNull();
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

describe('first-run routing', () => {
  it('opens the full-window survey before any first scan', async () => {
    const scans: unknown[] = [];
    const base = createMockBridge();
    const bridge: ScullBridge = {
      ...base,
      status: async () => ({ ...MOCK_DATA.status, state: 'no_scan', last_scan: null }),
      scan: async (p) => { scans.push(p); return MOCK_DATA.scanReport; },
    };
    const { wrapper, router } = await mountApp('/', bridge);
    await vi.waitFor(() => expect(router.currentRoute.value.path).toBe('/onboarding'));
    expect(wrapper.find('[data-testid="sidebar"]').exists()).toBe(false);
    expect(wrapper.get('h1').text()).toBe('What do you build?');
    expect(scans).toEqual([]);
    wrapper.unmount();
  });
});
