// Settings > General > Appearance (P5-UI-01): System / Light / Dark, saved on this machine,
// never through the settings writer (it's not scull config).
import { flushPromises, mount } from '@vue/test-utils';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { createMemoryHistory, createRouter } from 'vue-router';
import { BRIDGE_KEY } from '../../bridge';
import { createMockBridge } from '../../bridge/mock';
import { createMockSettingsWriter } from '../../components/settings/bridge';
import { THEME_KEY, useTheme } from '../../lib/theme';
import { routes } from '../../navigation';
import SettingsScreen from './SettingsScreen.vue';

afterEach(() => {
  useTheme().setTheme('system');
  localStorage.clear();
});

async function mountScreen() {
  const router = createRouter({ history: createMemoryHistory(), routes });
  router.push('/settings');
  await router.isReady();
  const writer = createMockSettingsWriter();
  const save = vi.spyOn(writer, 'save');
  const w = mount(SettingsScreen, {
    props: { writer },
    global: { plugins: [router], provide: { [BRIDGE_KEY as symbol]: createMockBridge() } },
  });
  await flushPromises();
  await vi.waitFor(() => expect(w.find('[data-testid="appearance"]').exists()).toBe(true));
  return { w, save };
}

describe('Settings: Appearance', () => {
  it('offers System, Light and Dark with System chosen by default', async () => {
    const { w } = await mountScreen();
    const group = w.get('[role="group"][aria-label="Theme"]');
    expect(group.findAll('button').map((b) => b.text())).toEqual(['System', 'Light', 'Dark']);
    expect(group.get('[aria-pressed="true"]').text()).toBe('System');
  });

  it('picking Dark applies and saves it locally, not through the settings writer', async () => {
    const { w, save } = await mountScreen();
    const group = w.get('[role="group"][aria-label="Theme"]');
    await group.findAll('button').find((b) => b.text() === 'Dark')!.trigger('click');
    expect(document.documentElement.dataset.theme).toBe('dark');
    expect(localStorage.getItem(THEME_KEY)).toBe('dark');
    expect(w.get('[role="group"][aria-label="Theme"] [aria-pressed="true"]').text()).toBe('Dark');
    expect(save).not.toHaveBeenCalled();

    await group.findAll('button').find((b) => b.text() === 'System')!.trigger('click');
    expect(document.documentElement.dataset.theme).toBeUndefined();
    expect(localStorage.getItem(THEME_KEY)).toBeNull();
  });
});
