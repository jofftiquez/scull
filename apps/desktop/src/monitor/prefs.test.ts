// Tray monitoring preferences: what the Settings keys mean for the monitor, local storage, and
// the start-up handshake (settings + saved memory, then snapshots kept).
import { beforeEach, describe, expect, it } from 'vitest';
import {
  STORAGE_KEYS,
  currentSettings,
  isMonitorKey,
  keepSnapshot,
  loadMemory,
  loadPrefs,
  localOffsetSecs,
  saveMonitorPref,
  settingsFrom,
  startMonitorSync,
} from './prefs';
import { mockMonitor, nextSlot } from './mock';

function memStore() {
  const m = new Map<string, string>();
  return {
    m,
    getItem: (k: string) => m.get(k) ?? null,
    setItem: (k: string, v: string) => void m.set(k, v),
  };
}

beforeEach(() => mockMonitor.reset());

describe('monitor preferences', () => {
  it('defaults are quiet: alerts on, digest off, config alert_days', () => {
    const s = settingsFrom({}, null, 3600);
    expect(s).toMatchObject({
      alerts: true,
      interval_secs: 3600,
      cooldown_secs: 86400,
      min_severity: 'warning',
      muted_kinds: [],
      alert_days: null,
      paused_until: null,
      digest: { enabled: false, weekday: 0, hour: 9, utc_offset_secs: 3600 },
    });
  });

  it('maps the Settings keys', () => {
    const s = settingsFrom(
      {
        'app.weekly_digest': true,
        'app.sudden_growth': false,
        'app.alert_days': 7,
        'app.monitor_every': '15m',
        'app.alert_cooldown': '7d',
      },
      1234,
      -25200,
    );
    expect(s.digest).toMatchObject({ enabled: true, utc_offset_secs: -25200 });
    expect(s.muted_kinds).toEqual(['growth_spike', 'free_space_drop']);
    expect(s.alert_days).toBe(7);
    expect(s.interval_secs).toBe(900);
    expect(s.cooldown_secs).toBe(7 * 86400);
    expect(s.paused_until).toBe(1234);
    expect(isMonitorKey('app.weekly_digest')).toBe(true);
    expect(isMonitorKey('app.caution')).toBe(false);
  });

  it('stores only what changed and ignores junk', () => {
    const st = memStore();
    st.setItem(STORAGE_KEYS.prefs, JSON.stringify({ 'app.weekly_digest': true, 'app.alert_days': { x: 1 }, other: 1 }));
    expect(loadPrefs(st)).toEqual({ 'app.weekly_digest': true });
    st.setItem(STORAGE_KEYS.prefs, '{not json');
    expect(loadPrefs(st)).toEqual({});
    expect(loadMemory(st)).toBeNull();
  });

  it('local offset is east-positive seconds', () => {
    const d = { getTimezoneOffset: () => -480 } as Date; // UTC+8
    expect(localOffsetSecs(d)).toBe(28800);
  });

  it('saving a preference reconfigures the monitor', async () => {
    const st = memStore();
    await saveMonitorPref(mockMonitor.control, 'app.weekly_digest', true, st);
    expect(loadPrefs(st)).toEqual({ 'app.weekly_digest': true });
    const status = mockMonitor.status();
    expect(status.configured).toBe(true);
    expect(status.settings.digest?.enabled).toBe(true);
    expect(status.next_digest).toBeGreaterThan(Date.now() / 1000);
    expect(currentSettings(st).digest?.enabled).toBe(true);
  });

  it('start-up sends settings and saved memory, then keeps snapshots', async () => {
    const st = memStore();
    st.setItem(STORAGE_KEYS.memory, JSON.stringify({ last_check: 100, notified: { 'low_space:/': { at: 100, severity: 'critical' } } }));
    const stop = await startMonitorSync(mockMonitor.control, st);
    expect(mockMonitor.status().notified).toBe(1);
    expect(mockMonitor.status().last_check).toBe(100);
    // A pause from the tray (or Settings) is kept for the next start.
    await mockMonitor.control.pause(3600);
    const paused = JSON.parse(st.m.get(STORAGE_KEYS.pausedUntil)!);
    expect(paused).toBeGreaterThan(Date.now() / 1000);
    expect(currentSettings(st).paused_until).toBe(paused);
    stop();
    keepSnapshot({ settings: { paused_until: null }, memory: {} }, st);
    expect(currentSettings(st).paused_until).toBeNull();
  });

  it('next digest slot: the next Monday 09:00 local after now', () => {
    const MON_9 = 1_793_610_000; // Monday 2026-11-02 09:00 UTC
    expect(nextSlot(MON_9 - 1, 0, 9, 0)).toBe(MON_9);
    expect(nextSlot(MON_9, 0, 9, 0)).toBe(MON_9 + 7 * 86400);
    expect(nextSlot(MON_9 - 10 * 3600, 0, 9, 8 * 3600)).toBe(MON_9 - 8 * 3600);
  });
});
