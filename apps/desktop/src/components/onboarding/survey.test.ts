import { flushPromises, mount } from '@vue/test-utils';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { createMemoryHistory, createRouter } from 'vue-router';
import { BRIDGE_KEY, type ScanParams, type ScullBridge } from '../../bridge';
import { createMockBridge, MOCK_DATA } from '../../bridge/mock';
import { routes } from '../../navigation';
import OnboardingScreen from '../../screens/Onboarding/OnboardingScreen.vue';
import { ONBOARDING_KEY, createMockOnboardingBridge, MOCK_DETECTION, type OnboardingBridge } from './bridge';
import { loadSetup } from './prefs';

beforeEach(() => localStorage.clear());
afterEach(() => { vi.restoreAllMocks(); document.body.innerHTML = ''; });

async function screen(overrides: Partial<OnboardingBridge> = {}, core: ScullBridge = createMockBridge()) {
  const router = createRouter({ history: createMemoryHistory(), routes });
  await router.push('/onboarding');
  const w = mount(OnboardingScreen, { attachTo: document.body, global: {
    plugins: [router], provide: { [BRIDGE_KEY as symbol]: core, [ONBOARDING_KEY as symbol]: { ...createMockOnboardingBridge(), ...overrides } },
  } });
  await flushPromises();
  return { w, router };
}

async function review(w: Awaited<ReturnType<typeof screen>>['w']) {
  for (let i = 0; i < 5; i++) await w.get('[data-testid="survey-next"]').trigger('click');
}

describe('six-step onboarding survey', () => {
  it('keeps removed folder suggestions hidden when returning to the folder step', async () => {
    const { w } = await screen();
    await w.get('[data-testid="survey-next"]').trigger('click');
    await w.get('button[aria-label="Remove ~/code"]').trigger('click');
    await w.get('[data-testid="survey-back"]').trigger('click');
    await w.get('[data-testid="survey-next"]').trigger('click');
    expect(w.find('button[aria-label="Remove ~/code"]').exists()).toBe(false);
    w.unmount();
  });

  it('shows one question at a time, keeps answers when going back, and edits from review', async () => {
    const { w } = await screen();
    expect(w.get('h1').text()).toBe('What do you build?');
    expect(w.get('[data-testid="survey-progress"]').text()).toContain('1 of 6');
    expect(w.find('[data-testid="start-scan"]').exists()).toBe(false);
    await w.get('input[value="mobile"]').setValue(true);
    await w.get('[data-testid="survey-next"]').trigger('click');
    expect(w.get('h1').text()).toBe('Where do your projects live?');
    await w.get('[data-testid="survey-back"]').trigger('click');
    expect((w.get('input[value="mobile"]').element as HTMLInputElement).checked).toBe(true);
    await review(w);
    expect(w.get('h1').text()).toBe('Ready for your first scan?');
    expect(w.findAll('[data-testid="review-row"]')).toHaveLength(5);
    await w.get('[data-testid="edit-step-2"]').trigger('click');
    expect(w.get('h1').text()).toBe('Where do your projects live?');
    w.unmount();
  });

  it('blocks empty choices and never scans while choosing or navigating', async () => {
    const scans: ScanParams[] = [];
    const base = createMockBridge();
    const { w } = await screen({}, { ...base, scan: async (p) => { scans.push(p); return MOCK_DATA.scanReport; } });
    for (const checkbox of w.findAll('input[type="checkbox"]')) await checkbox.setValue(false);
    expect(w.get('[data-testid="survey-next"]').attributes('disabled')).toBeDefined();
    await w.get('input[value="other"]').setValue(true);
    await w.get('[data-testid="survey-next"]').trigger('click');
    for (const checkbox of w.findAll('[data-testid="folder"] input')) await checkbox.setValue(false);
    expect(w.get('[data-testid="survey-next"]').attributes('disabled')).toBeDefined();
    expect(scans).toEqual([]);
    w.unmount();
  });

  it('does not overwrite a manual answer when detection finishes late', async () => {
    let finish!: (value: typeof MOCK_DETECTION) => void;
    const { w } = await screen({ detect: () => new Promise((r) => { finish = r; }) });
    await w.get('input[value="mobile"]').setValue(true);
    finish(MOCK_DETECTION);
    await flushPromises();
    expect((w.get('input[value="mobile"]').element as HTMLInputElement).checked).toBe(true);
    w.unmount();
  });

  it('requires native home context rather than trusting guessed detection', async () => {
    const { w } = await screen({ context: async () => ({ home: null }) });
    await w.get('[data-testid="survey-next"]').trigger('click');
    expect(w.get('[data-testid="survey-next"]').attributes('disabled')).toBeDefined();
    expect(w.get('[role="alert"]').text()).toContain('home folder');
    w.unmount();
  });

  it('saves once, scans chosen roots only, and shows completion', async () => {
    const scans: ScanParams[] = [];
    const base = createMockBridge();
    let complete!: () => void;
    const { w } = await screen({}, { ...base, scan: (p) => { scans.push(p); return new Promise((r) => { complete = () => r(MOCK_DATA.scanReport); }); } });
    await review(w);
    await w.get('[data-testid="start-scan"]').trigger('click');
    expect(scans).toEqual([{ roots: ['/Users/ada/code', '/Users/ada/work'], allow_broad_roots: false }]);
    expect(w.find('[data-testid="survey-back"]').exists()).toBe(false);
    expect(w.get('[role="progressbar"]').attributes('aria-valuenow')).toBeUndefined();
    expect(loadSetup()!.answers.roots).toEqual(['/Users/ada/code', '/Users/ada/work']);
    complete();
    await flushPromises();
    expect(w.get('[data-testid="scan"]').text()).toContain('Scan complete');
    expect(w.find('[data-testid="open-overview"]').exists()).toBe(true);
    w.unmount();
  });

  it('offers an explicit one-time scan after storage failure', async () => {
    const scans: ScanParams[] = [];
    const { w } = await screen({}, { ...createMockBridge(), scan: async (p) => { scans.push(p); return MOCK_DATA.scanReport; } });
    await review(w);
    vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => { throw new Error('storage full'); });
    await w.get('[data-testid="start-scan"]').trigger('click');
    expect(scans).toEqual([]);
    expect(w.get('[data-testid="save-note"]').text()).toContain('could not be remembered');
    await w.get('[data-testid="scan-once"]').trigger('click');
    await flushPromises();
    expect(scans).toHaveLength(1);
    w.unmount();
  });

  it('keeps failed scans editable and cancelled scans explicitly partial', async () => {
    let result: 'fail' | 'cancel' = 'fail';
    const { w } = await screen({}, { ...createMockBridge(), scan: async () => {
      if (result === 'fail') throw new Error('Folder is no longer available');
      return { ...MOCK_DATA.scanReport, cancelled: true };
    } });
    await review(w);
    await w.get('[data-testid="start-scan"]').trigger('click');
    await flushPromises();
    expect(w.get('[role="alert"]').text()).toContain('Folder is no longer available');
    expect(w.find('[data-testid="edit-folders"]').exists()).toBe(true);
    result = 'cancel';
    await w.get('[data-testid="retry-scan"]').trigger('click');
    await flushPromises();
    expect(w.get('[data-testid="scan"]').text()).toContain('partial');
    w.unmount();
  });

  it('lets setup wait without saving a draft or starting a scan', async () => {
    const scans: ScanParams[] = [];
    const { w, router } = await screen({}, { ...createMockBridge(), scan: async (p) => { scans.push(p); return MOCK_DATA.scanReport; } });
    await w.get('[data-testid="skip"]').trigger('click');
    await flushPromises();
    await vi.waitFor(() => expect(router.currentRoute.value.path).toBe('/'));
    expect(loadSetup()).toBeNull();
    expect(scans).toEqual([]);
    w.unmount();
  });
});
