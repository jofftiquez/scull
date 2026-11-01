import { flushPromises, mount } from '@vue/test-utils';
import { describe, expect, it, vi } from 'vitest';
import { createMemoryHistory, createRouter } from 'vue-router';
import { BRIDGE_KEY, type ScullBridge, type SettingsView } from '../../bridge';
import { createMockBridge } from '../../bridge/mock';
import { designSettings } from '../../bridge/settingsMock';
import { createMockSettingsWriter, createPendingSettingsWriter, type SettingsWriter } from '../../components/settings/bridge';
import { routes } from '../../navigation';
import SettingsScreen from './SettingsScreen.vue';

async function mountScreen(opts: { writer?: SettingsWriter; bridge?: ScullBridge; tab?: string } = {}) {
  const router = createRouter({ history: createMemoryHistory(), routes });
  router.push(opts.tab ? `/settings?tab=${opts.tab}` : '/settings');
  await router.isReady();
  const w = mount(SettingsScreen, {
    props: { writer: opts.writer ?? createMockSettingsWriter() },
    global: { plugins: [router], provide: { [BRIDGE_KEY as symbol]: opts.bridge ?? createMockBridge() } },
  });
  await flushPromises();
  await vi.waitFor(() => expect(w.find('[role="tablist"]').exists()).toBe(true));
  return w;
}

type W = Awaited<ReturnType<typeof mountScreen>>;
const sw = (w: W, label: string) => w.get(`[role="switch"][aria-label="${label}"]`);
const tabTo = async (w: W, id: string) => {
  await w.get(`#settings-tab-${id}`).trigger('click');
  await flushPromises();
};

describe('Settings screen', () => {
  it('has the seven tabs and opens on General with the configured scan folders', async () => {
    const w = await mountScreen();
    expect(w.findAll('[role="tab"]').map((t) => t.text())).toEqual([
      'General',
      'Drives',
      'Safety',
      'Alerts',
      'AI agents',
      'Data & privacy',
      'About',
    ]);
    expect(w.get('[aria-selected="true"]').text()).toBe('General');
    expect(w.get('[data-testid="roots"]').text()).toContain('~/code');
    expect(w.get('[data-testid="roots"]').text()).toContain('~/.cache');
    expect(w.findAll('[data-testid="origin"]')[0]!.text()).toBe('config.toml');
  });

  it('every tab renders its panel', async () => {
    const w = await mountScreen();
    for (const id of ['drives', 'safety', 'alerts', 'agents', 'data', 'about', 'general']) {
      await tabTo(w, id);
      expect(w.find(`[data-testid="panel-${id}"]`).exists()).toBe(true);
      expect(w.get(`#settings-tab-${id}`).attributes('aria-selected')).toBe('true');
    }
  });

  it('opens the tab named in the URL and moves with the arrow keys', async () => {
    const w = await mountScreen({ tab: 'drives' });
    expect(w.find('[data-testid="panel-drives"]').exists()).toBe(true);
    expect(w.findAll('[data-testid="drive-row"]')).toHaveLength(3);
    expect(sw(w, 'Scan Backup (Time Machine)').attributes('disabled')).toBeDefined();
    await w.get('#settings-tab-drives').trigger('keydown', { key: 'ArrowDown' });
    await flushPromises();
    expect(w.find('[data-testid="panel-safety"]').exists()).toBe(true);
  });

  it('a toggle slides, saves through the stub and shows the Saved pill', async () => {
    const writer = createMockSettingsWriter();
    const w = await mountScreen({ writer });
    expect(sw(w, 'Open at login').attributes('aria-checked')).toBe('true');
    await sw(w, 'Open at login').trigger('click');
    await vi.waitFor(() => expect(w.get('[data-testid="saved"]').text()).toBe('Saved ✓'));
    expect(sw(w, 'Open at login').attributes('aria-checked')).toBe('false');
    expect(writer.calls).toEqual(['save app.open_at_login=false']);
  });

  it('segments save their value', async () => {
    const writer = createMockSettingsWriter();
    const w = await mountScreen({ writer, tab: 'safety' });
    const group = w.get('[role="group"][aria-label="Recently used means"]');
    expect(group.get('[aria-pressed="true"]').text()).toBe('24 hours');
    await group.findAll('button').find((b) => b.text() === '7 days')!.trigger('click');
    await vi.waitFor(() => expect(writer.calls).toEqual(['save scan.activity_window="168h"']));
    expect(w.get('[role="group"][aria-label="Recently used means"] [aria-pressed="true"]').text()).toBe('7 days');
  });

  it('removes and adds scan folders (validated) through the stub', async () => {
    const writer = createMockSettingsWriter();
    const w = await mountScreen({ writer });
    await w.get('[aria-label="Remove ~/.cache"]').trigger('click');
    await vi.waitFor(() => expect(w.get('[data-testid="roots"]').text()).not.toContain('~/.cache'));
    await w.findAll('button').find((b) => b.text() === 'Add folder')!.trigger('click');
    await w.get('input[aria-label="Folder to scan"]').setValue('relative/path');
    await w.get('form').trigger('submit');
    expect(w.get('[data-testid="save-error"]').text()).toMatch(/absolute/);
    await w.get('input[aria-label="Folder to scan"]').setValue('~/projects');
    await w.get('form').trigger('submit');
    await vi.waitFor(() => expect(w.get('[data-testid="roots"]').text()).toContain('~/projects'));
    expect(writer.calls).toEqual(['save scan.roots=["~/code","~/work"]', 'save scan.roots=["~/code","~/work","~/projects"]']);
  });

  it('AI agents: MCP settings with origins; destructive is locked off; redaction on', async () => {
    const w = await mountScreen({ tab: 'agents' });
    expect(sw(w, 'Let agents use scull').attributes('aria-checked')).toBe('true');
    const destructive = sw(w, 'Let agents prepare and run cleanup plans');
    expect(destructive.attributes('aria-checked')).toBe('false');
    expect(destructive.attributes('disabled')).toBeDefined();
    expect(sw(w, 'Hide my home folder in paths').attributes('aria-checked')).toBe('true');
    expect(sw(w, 'Hash project names').attributes('aria-checked')).toBe('false');
    expect(w.get('[data-testid="max-results"]').text()).toBe('1000');
    const origins = w.findAll('[data-testid="origin"]').map((o) => o.text());
    expect(origins).toEqual(['config.toml', 'default', 'default', 'default', 'default']);
    expect(w.findAll('[data-testid="client"]')).toHaveLength(3);
  });

  it('inside Tauri a change is refused: the switch reverts and the reason is shown', async () => {
    const w = await mountScreen({ writer: createPendingSettingsWriter(), tab: 'agents' });
    await sw(w, 'Hash project names').trigger('click');
    await vi.waitFor(() => expect(w.find('[data-testid="save-error"]').exists()).toBe(true));
    expect(w.get('[data-testid="save-error"]').text()).toMatch(/nothing was changed/);
    expect(sw(w, 'Hash project names').attributes('aria-checked')).toBe('false');
    expect(w.get('[data-testid="saved"]').text()).toBe('');
  });

  it('Data & privacy shows scull data show info read-only', async () => {
    const w = await mountScreen({ tab: 'data' });
    expect(w.get('[data-testid="footprint"]').text()).toBe('212.0 MiB of history');
    await w.get('[aria-controls="what-scull-knows"]').trigger('click');
    const known = w.get('[data-testid="what-scull-knows"]').text();
    expect(known).toContain('Database: readable');
    expect(known).toContain('scull.db-wal');
  });

  it('About shows the version and the config file state', async () => {
    const w = await mountScreen({ tab: 'about' });
    expect(w.get('[data-testid="version"]').text()).toMatch(/^v\d+\.\d+\.\d+ · MIT OR Apache-2.0/);
    expect(w.get('[data-testid="panel-about"]').text()).toContain('~/Library/Application Support/scull/config.toml');
    expect(sw(w, 'Check for updates').attributes('aria-checked')).toBe('false');
  });

  it('an invalid config file is reported, with defaults shown', async () => {
    const bad: SettingsView = { ...designSettings(), config_state: 'invalid', config_error: 'config.toml: expected `]` at line 1' };
    const w = await mountScreen({ bridge: { ...createMockBridge(), settings: async () => bad } });
    expect(w.get('[data-testid="config-error"]').text()).toContain('expected `]`');
  });

  it('a bridge error is shown', async () => {
    const router = createRouter({ history: createMemoryHistory(), routes });
    router.push('/settings');
    await router.isReady();
    const bridge = { ...createMockBridge(), settings: async () => Promise.reject({ kind: 'config', message: 'no config dir' }) };
    const w = mount(SettingsScreen, { global: { plugins: [router], provide: { [BRIDGE_KEY as symbol]: bridge } } });
    await vi.waitFor(() => expect(w.find('[role="alert"]').exists()).toBe(true));
    expect(w.get('[role="alert"]').text()).toContain('no config dir');
  });
});
