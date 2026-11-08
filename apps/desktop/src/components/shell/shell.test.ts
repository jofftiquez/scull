import { flushPromises, mount } from '@vue/test-utils';
import { describe, expect, it, vi } from 'vitest';
import { createMemoryHistory, createRouter } from 'vue-router';
import App from '../../App.vue';
import { BRIDGE_KEY } from '../../bridge';
import { createMockBridge } from '../../bridge/mock';
import type { Drive, ScullBridge } from '../../bridge';
import { NAV_ITEMS, routes } from '../../navigation';

async function mountApp(path = '/', bridge: ScullBridge = createMockBridge()) {
  const router = createRouter({ history: createMemoryHistory(), routes });
  router.push(path);
  await router.isReady();
  const wrapper = mount(App, { global: { plugins: [router], provide: { [BRIDGE_KEY as symbol]: bridge } } });
  await flushPromises();
  return { wrapper, router };
}

describe('app shell', () => {
  it('renders the design 01 sidebar: nav on top, search and drives at the bottom', async () => {
    const { wrapper } = await mountApp();
    const nav = wrapper.get('[data-testid="sidebar"]');
    expect(nav.attributes('aria-label')).toBe('Primary');
    const labels = nav.findAll('a.nav-item').map((a) => a.text());
    expect(labels).toEqual(NAV_ITEMS.map((i) => i.label));
    expect(nav.find('[role="separator"]').exists()).toBe(true);
    expect(nav.find('input[type="search"]').attributes('placeholder')).toBe('Search paths, projects…');
    expect(nav.text()).not.toContain('Supporter');
    expect(wrapper.get('[data-testid="header"]').text()).not.toMatch(/scanned|reclaimable|Show regenerable/);
    expect(wrapper.get('[data-testid="rescan"]').text()).toContain('Scan');
    expect(nav.find('[data-testid="drives"]').exists()).toBe(true);
  });

  it('marks the current page', async () => {
    const { wrapper } = await mountApp('/caches');
    const current = wrapper.findAll('a[aria-current="page"]');
    expect(current).toHaveLength(1);
    expect(current[0]!.text()).toContain('Caches');
  });

  it('navigates between screens', async () => {
    const { wrapper, router } = await mountApp('/');
    await router.push('/worktrees');
    await flushPromises();
    await vi.waitFor(() => expect(wrapper.find('h1').text()).toContain('Git Worktrees'));
  });

  it('shows drives from the bridge, unknown sizes as "size unknown"', async () => {
    const drives: Drive[] = [
      { mount_point: '/', label: 'System', kind: 'local', scan_policy: 'scan', holds_home: true, total_bytes: 100, free_bytes: 25 },
      { mount_point: '/mnt/x', label: 'x', kind: 'removable', scan_policy: 'scan', holds_home: false, total_bytes: null, free_bytes: null },
      { mount_point: '//nas/a', label: 'a', kind: 'network', scan_policy: 'off_by_default', holds_home: false, total_bytes: null, free_bytes: null },
    ];
    const bridge: ScullBridge = { ...createMockBridge(), drives: async () => drives };
    const { wrapper } = await mountApp('/', bridge);
    const rows = wrapper.findAll('[data-testid="drive"]');
    expect(rows).toHaveLength(2);
    expect(rows[0]!.text()).toContain('75%');
    expect(rows[1]!.text()).toContain('size unknown');
    expect(rows[1]!.text()).not.toMatch(/\b0 B\b/);
    expect(wrapper.get('[data-testid="drive-not-scanned"]').text()).toContain('not scanned unless you opt in');
  });

  it('survives a failing drive listing', async () => {
    const bridge: ScullBridge = { ...createMockBridge(), drives: () => Promise.reject(new Error('x')) };
    const { wrapper } = await mountApp('/', bridge);
    expect(wrapper.get('[data-testid="drives"]').text()).toContain('Drive list unavailable');
  });

  it('every nav target is a real route', () => {
    for (const item of NAV_ITEMS) {
      expect(routes.some((r) => r.path === item.path)).toBe(true);
    }
  });
});
