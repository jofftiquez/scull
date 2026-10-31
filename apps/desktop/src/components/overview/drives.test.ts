import { flushPromises, mount } from '@vue/test-utils';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { createMemoryHistory, createRouter } from 'vue-router';
import App from '../../App.vue';
import { BRIDGE_KEY, type Drive, type ScullBridge, type VolumeSampleView } from '../../bridge';
import { createMockBridge, MOCK_DRIVES } from '../../bridge/mock';
import { routes } from '../../navigation';
import { core } from '../../state/core';
import { DESIGN_CANDIDATES, DESIGN_SSD_CANDIDATES, designScenarioBridge, wantsSecondDrive } from './designScenario';
import {
  ALL_DRIVES,
  candidatesOn,
  chosenDrive,
  driveChoice,
  driveContext,
  driveForecast,
  driveOf,
  driveOptions,
  onMount,
  samplesOf,
} from './drives';

const GiB = 1024 ** 3;
const [HD, SSD, BACKUP] = MOCK_DRIVES as [Drive, Drive, Drive];
const ALL = [...DESIGN_CANDIDATES, ...DESIGN_SSD_CANDIDATES];
const gib = (b: number) => Math.round((b / GiB) * 10) / 10;

function series(id: string, free0: number, perDay: number, now = 2_000_000_000): VolumeSampleView[] {
  return Array.from({ length: 8 }, (_, i) => ({
    volume_id: id,
    total_bytes: 1000 * GiB,
    free_bytes: Math.round((free0 + (7 - i) * perDay) * GiB),
    sampled_at: now - (7 - i) * 86400,
  }));
}

describe('drive matching', () => {
  it('matches mount points by path segments, longest first; Windows case-insensitively', () => {
    expect(onMount('/Volumes/Dev SSD/x', '/Volumes/Dev SSD')).toBe(true);
    expect(onMount('/Volumes/Dev SSDx', '/Volumes/Dev SSD')).toBe(false);
    expect(onMount('c:\\Users\\a', 'C:\\')).toBe(true);
    expect(onMount('C:\\x', '/')).toBe(false);
    expect(driveOf('/Volumes/Dev SSD/vm-images/a', MOCK_DRIVES)).toBe(SSD);
    expect(driveOf('/Users/ada/code', MOCK_DRIVES)).toBe(HD);
    expect(driveOf('D:\\x', MOCK_DRIVES)).toBeNull();
  });
});

describe('driveOptions', () => {
  it('lists All, then the home drive, then other scanned drives; never the backup (D-22)', () => {
    const opts = driveOptions(MOCK_DRIVES, ALL);
    expect(opts.map((o) => [o.id, o.label, o.kind, gib(o.bytes), o.count, o.used])).toEqual([
      [ALL_DRIVES, 'All drives', null, 338.7, 20, null],
      ['/', 'Macintosh HD', null, 186.0, 16, 92],
      ['/Volumes/Dev SSD', 'Dev SSD', 'external', 152.7, 4, 61],
    ]);
    expect(opts.some((o) => o.drive === BACKUP)).toBe(false);
  });

  it('shows a scanned drive with nothing found yet, and hides the switcher with one drive', () => {
    expect(driveOptions(MOCK_DRIVES, DESIGN_CANDIDATES).map((o) => o.count)).toEqual([16, 16, 0]);
    expect(driveOptions([HD, BACKUP], DESIGN_CANDIDATES)).toEqual([]);
  });

  it('lists a network drive only once it holds scanned data (off by default)', () => {
    const nas: Drive = { mount_point: '/Volumes/NAS', label: 'NAS', kind: 'network', scan_policy: 'off_by_default', holds_home: false };
    expect(driveOptions([HD, nas], DESIGN_CANDIDATES)).toEqual([]);
    const onNas = { ...DESIGN_SSD_CANDIDATES[0]!, id: 'nas', path: '/Volumes/NAS/big.vhdx' };
    expect(driveOptions([HD, nas], [...DESIGN_CANDIDATES, onNas]).map((o) => [o.label, o.kind])).toEqual([
      ['All drives', null],
      ['Macintosh HD', null],
      ['NAS', 'network'],
    ]);
  });

  it('filters candidates per drive; an unknown choice falls back to All', () => {
    const opts = driveOptions(MOCK_DRIVES, ALL);
    expect(candidatesOn(ALL, SSD, MOCK_DRIVES).map((c) => c.id)).toEqual(DESIGN_SSD_CANDIDATES.map((c) => c.id));
    expect(candidatesOn(ALL, null, MOCK_DRIVES)).toHaveLength(20);
    expect(chosenDrive('/Volumes/Dev SSD', opts)).toBe(SSD);
    expect(chosenDrive('/Volumes/Gone', opts)).toBeNull();
    expect(chosenDrive(ALL_DRIVES, opts)).toBeNull();
  });
});

describe('per-drive forecast', () => {
  const samples = [...series('/', 41.2, 3.5), ...series('/Volumes/Dev SSD', 390, 14)];

  it("uses each drive's own samples", () => {
    expect(samplesOf(SSD, samples, MOCK_DRIVES)).toHaveLength(8);
    expect(driveForecast(HD, MOCK_DRIVES, samples)).toMatchObject({ days: 9, drive: HD });
    expect(driveForecast(SSD, MOCK_DRIVES, samples)).toMatchObject({ days: 27, drive: SSD });
    expect(driveForecast(BACKUP, MOCK_DRIVES, samples)).toBeNull();
  });

  it('for All drives, shows the drive that fills first', () => {
    expect(driveForecast(null, MOCK_DRIVES, samples)).toMatchObject({ days: 9, drive: HD });
    const ssdFirst = [...series('/', 300, 1), ...series('/Volumes/Dev SSD', 50, 5)];
    expect(driveForecast(null, MOCK_DRIVES, ssdFirst)).toMatchObject({ days: 8, drive: SSD });
  });

  it('adds bytes freed by Quick Clean to the home drive only', () => {
    expect(driveForecast(HD, MOCK_DRIVES, samples, 35 * GiB)!.days).toBe(19);
    expect(driveForecast(null, MOCK_DRIVES, [...series('/', 300, 1), ...series('/Volumes/Dev SSD', 50, 5)], 100 * GiB)!.drive).toBe(SSD);
  });

  it("gives an unmatched single volume (a Windows volume GUID path) to the home drive, never a listed drive's", () => {
    const c: Drive = { mount_point: 'C:\\', label: 'C:', kind: 'local', scan_policy: 'scan', holds_home: true };
    const guid = series('\\\\?\\Volume{1234}\\', 41.2, 3.5);
    expect(samplesOf(c, guid, [c])).toHaveLength(8);
    expect(samplesOf(HD, series('/Volumes/Dev SSD', 390, 14), MOCK_DRIVES)).toEqual([]);
    expect(driveForecast(null, [], series('/', 41.2, 3.5))).toMatchObject({ days: 9, drive: null });
  });
});

describe('drive context', () => {
  it('says where cleanups are quarantined (D-22: one quarantine per drive)', () => {
    const opts = driveOptions(MOCK_DRIVES, ALL);
    expect(driveContext(SSD, opts)).toBe('Dev SSD · 390.0 GiB free · cleanups here are quarantined on Dev SSD');
    expect(driveContext(null, opts)).toBe('2 drives · each keeps its own forecast and quarantine');
    expect(driveContext({ ...SSD, free_bytes: null }, opts)).toContain('free space unknown');
  });

  it('the second drive is opt-in in the design scenario', () => {
    expect(wantsSecondDrive({ hash: '#/?scenario=design&drives=2' })).toBe(true);
    expect(wantsSecondDrive({ search: '?scenario=design' })).toBe(false);
  });
});

describe('overview drive switcher', () => {
  beforeEach(() => {
    Object.assign(core, { status: null, scanning: false, progress: null, error: null, report: null });
    // Reduced motion: the radar sweep ends as soon as the scan does.
    vi.stubGlobal('matchMedia', (q: string) => ({ matches: q.includes('reduce'), media: q, addEventListener: () => {}, removeEventListener: () => {} }));
  });
  afterEach(() => {
    vi.unstubAllGlobals();
    driveChoice.value = ALL_DRIVES;
    core.report = null;
    core.status = null;
    document.body.innerHTML = '';
  });

  async function mountOverview(bridge: ScullBridge) {
    const router = createRouter({ history: createMemoryHistory(), routes });
    router.push('/');
    await router.isReady();
    const w = mount(App, { attachTo: document.body, global: { plugins: [router], provide: { [BRIDGE_KEY as symbol]: bridge } } });
    await flushPromises();
    await vi.waitFor(() => expect(core.report).not.toBeNull(), { timeout: 3000 });
    await vi.waitFor(() => expect(w.get('[data-testid="rescan"]').text()).toBe('Rescan'), { timeout: 3000 });
    await vi.waitFor(() => expect(w.find('[data-testid="drive-switcher"]').exists()).toBe(true));
    return w;
  }

  it('switches the wheel, forecast, Quick Clean and quarantine line per drive', async () => {
    const w = await mountOverview(designScenarioBridge(createMockBridge(), 0, true));
    const opts = () => w.findAll('[data-testid="drive-option"]');
    expect(opts().map((o) => o.attributes('aria-label'))).toEqual([
      'All drives: 20 items, 338.7 GiB',
      'Macintosh HD: 16 items, 186.0 GiB, 92% used',
      'Dev SSD, external: 4 items, 152.7 GiB, 61% used',
    ]);
    expect(opts()[0]!.attributes('aria-pressed')).toBe('true');
    expect(w.get('[data-testid="center"]').text()).toContain('338.7 GiB');
    expect(w.get('[data-testid="forecast"]').text()).toContain('Macintosh HD full in about 9 days.');

    await opts()[2]!.trigger('click');
    await vi.waitFor(() => expect(w.get('[data-testid="center"]').text()).toContain('152.7 GiB'));
    expect(opts()[2]!.attributes('aria-pressed')).toBe('true');
    expect(w.findAll('path[data-kind="item"]')).toHaveLength(4);
    expect(w.get('[data-testid="forecast"]').text()).toContain('Dev SSD full in about 27 days.');
    expect(w.get('[data-testid="drive-context"]').text()).toBe('Dev SSD · 390.0 GiB free · cleanups here are quarantined on Dev SSD');
    expect(w.get('[data-testid="show-regenerable"]').text()).toBe('Show regenerable · 48.2 GiB');

    await opts()[1]!.trigger('click');
    await vi.waitFor(() => expect(w.get('[data-testid="center"]').text()).toContain('186.0 GiB'));
    expect(w.findAll('path[data-kind="item"]')).toHaveLength(16);
    w.unmount();
  });

  it('keeps the choice across visits and falls back to All when the drive is gone', async () => {
    driveChoice.value = '/Volumes/Gone';
    const w = await mountOverview(designScenarioBridge(createMockBridge(), 0, true));
    expect(w.findAll('[data-testid="drive-option"]')[0]!.attributes('aria-pressed')).toBe('false');
    expect(w.get('[data-testid="center"]').text()).toContain('338.7 GiB');
    expect(w.get('[data-testid="drive-context"]').text()).toContain('2 drives');
    w.unmount();
  });

  it('has no switcher on a one-drive machine', async () => {
    const base = designScenarioBridge(createMockBridge(), 0);
    const w = mount(App, {
      attachTo: document.body,
      global: {
        plugins: [createRouter({ history: createMemoryHistory(), routes })],
        provide: { [BRIDGE_KEY as symbol]: { ...base, drives: async () => [HD, BACKUP] } },
      },
    });
    await vi.waitFor(() => expect(core.report).not.toBeNull(), { timeout: 3000 });
    await flushPromises();
    expect(w.find('[data-testid="drive-switcher"]').exists()).toBe(false);
    w.unmount();
  });
});
