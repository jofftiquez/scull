import { flushPromises, mount } from '@vue/test-utils';
import { describe, expect, it, vi } from 'vitest';
import { createMemoryHistory, createRouter } from 'vue-router';
import { BRIDGE_KEY, type ScullBridge, type SettingsView } from '../../bridge';
import { createMockBridge } from '../../bridge/mock';
import { designSettings } from '../../bridge/settingsMock';
import { createMockSettingsWriter, createPendingSettingsWriter, type SettingsWriter } from '../../components/settings/bridge';
import { routes } from '../../navigation';
import SettingsScreen from './SettingsScreen.vue';
import { mockMonitor } from '../../monitor/mock';
import { STORAGE_KEYS } from '../../monitor/prefs';

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

  it('Alerts: a status refresh that comes back after a pause does not undo it', async () => {
    localStorage.clear();
    mockMonitor.reset();
    // Hold the refresh that follows a monitor setting change, like a slow bridge: it reads the
    // status when asked (not paused yet) and answers only when released.
    const mock = createMockBridge();
    const held: (() => void)[] = [];
    let hold = false;
    const bridge: ScullBridge = {
      ...mock,
      monitorStatus: async () => {
        const st = mockMonitor.status();
        if (hold) await new Promise<void>((r) => held.push(r));
        return st;
      },
    };
    const w = await mountScreen({ bridge, tab: 'alerts' });
    hold = true;
    await w.get('[aria-label="Look every"] button:first-child').trigger('click');
    await vi.waitFor(() => expect(held).toHaveLength(1));
    hold = false;
    await w.findAll('[data-testid="monitoring"] button').find((b) => b.text() === '1 hour')!.trigger('click');
    await vi.waitFor(() => expect(w.get('[data-testid="pause-state"]').text()).toMatch(/^Paused until /));
    held.forEach((r) => r());
    await flushPromises();
    expect(w.get('[data-testid="pause-state"]').text()).toMatch(/^Paused until /);
    expect(w.findAll('[data-testid="monitoring"] button').some((b) => b.text() === 'Resume')).toBe(true);
    mockMonitor.reset();
    localStorage.clear();
  });

  it('Alerts: monitor settings save even with the refusing writer, and pause/send now work', async () => {
    localStorage.clear();
    mockMonitor.reset();
    // The writer the app uses inside Tauri refuses config changes; monitor keys still save.
    const w = await mountScreen({ writer: createPendingSettingsWriter(), tab: 'alerts' });
    expect(sw(w, 'Weekly digest').attributes('aria-checked')).toBe('false');
    expect(w.get('[data-testid="next-digest"]').text()).toContain('Turn on Weekly digest');
    await sw(w, 'Weekly digest').trigger('click');
    await vi.waitFor(() => expect(w.get('[data-testid="saved"]').text()).toBe('Saved ✓'));
    expect(w.find('[data-testid="save-error"]').exists()).toBe(false);
    expect(sw(w, 'Weekly digest').attributes('aria-checked')).toBe('true');
    expect(JSON.parse(localStorage.getItem(STORAGE_KEYS.prefs)!)).toEqual({ 'app.weekly_digest': true });
    expect(mockMonitor.status().settings.digest?.enabled).toBe(true);
    await vi.waitFor(() => expect(w.get('[data-testid="next-digest"]').text()).toMatch(/^Next digest (today|tomorrow|\w{3}) \d\d:\d\d\.$/));

    // Sudden growth off mutes growth alerts; the look-every choice sets the interval.
    await sw(w, 'Sudden growth').trigger('click');
    await vi.waitFor(() => expect(mockMonitor.status().settings.muted_kinds).toEqual(['growth_spike', 'free_space_drop']));
    await flushPromises();
    await w.get('[aria-label="Look every"] button:first-child').trigger('click');
    await vi.waitFor(() => expect(mockMonitor.status().settings.interval_secs).toBe(900));

    // Pause for an hour, then resume.
    const pause = w.findAll('[data-testid="monitoring"] button').find((b) => b.text() === '1 hour')!;
    await pause.trigger('click');
    await vi.waitFor(() => expect(w.get('[data-testid="pause-state"]').text()).toMatch(/^Paused until /));
    expect(mockMonitor.status().paused).toBe(true);
    const resume = w.findAll('[data-testid="monitoring"] button').find((b) => b.text() === 'Resume')!;
    await resume.trigger('click');
    await vi.waitFor(() => expect(w.get('[data-testid="pause-state"]').text()).toBe('Alerts are on.'));

    // Send now: the digest is prepared (the mock shows no notification).
    const send = w.findAll('[data-testid="monitoring"] button').find((b) => b.text() === 'Send now')!;
    await send.trigger('click');
    await vi.waitFor(() => expect(mockMonitor.sent).toHaveLength(1));
    expect(mockMonitor.sent[0]!.digest?.title).toBe('Your week in scull');

    // A remount reads the stored choices back.
    const again = await mountScreen({ tab: 'alerts' });
    expect(sw(again, 'Weekly digest').attributes('aria-checked')).toBe('true');
    expect(sw(again, 'Sudden growth').attributes('aria-checked')).toBe('false');
    localStorage.clear();
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
