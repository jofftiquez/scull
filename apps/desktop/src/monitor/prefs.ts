// Tray monitoring preferences and memory, kept in the main window's local storage (app state
// on this computer only: no file is written, nothing leaves the machine). The Settings screen
// edits the `app.*` keys below; startMonitorSync() sends them to the monitor at start and keeps
// the snapshots it emits.
import type { AlertKind, MonitorMemory, MonitorSettings, MonitorSnapshot } from '../bridge/types';
import { durationSecs } from '../components/settings/model';
import { MONITOR_SETTING_DEFAULTS } from './mock';
import type { MonitorControl } from './control';

/** Settings keys that configure the monitor (the rest of `app.*` has no consumer yet). */
export const MONITOR_KEYS = [
  'app.alert_days',
  'app.sudden_growth',
  'app.weekly_digest',
  'app.monitor_every',
  'app.alert_cooldown',
] as const;
export type MonitorKey = (typeof MONITOR_KEYS)[number];
export type MonitorPrefValue = boolean | number | string;

export function isMonitorKey(k: string): k is MonitorKey {
  return (MONITOR_KEYS as readonly string[]).includes(k);
}

export const STORAGE_KEYS = {
  prefs: 'scull.monitor.prefs',
  memory: 'scull.monitor.memory',
  pausedUntil: 'scull.monitor.paused_until',
} as const;

/** Alert kinds "Sudden growth" covers (PRO-04). */
export const GROWTH_KINDS: readonly AlertKind[] = ['growth_spike', 'free_space_drop'];

type Store = Pick<Storage, 'getItem' | 'setItem'>;

function storage(): Store | null {
  try {
    return globalThis.localStorage ?? null;
  } catch {
    return null;
  }
}

function read<T>(s: Store | null, key: string): T | null {
  try {
    const v = s?.getItem(key);
    return v ? (JSON.parse(v) as T) : null;
  } catch {
    return null;
  }
}

function write(s: Store | null, key: string, value: unknown) {
  try {
    s?.setItem(key, JSON.stringify(value));
  } catch {
    // Private mode or a full storage: the monitor keeps working with what it has.
  }
}

/** The monitor preferences the user changed (only those: defaults stay defaults). */
export function loadPrefs(s: Store | null = storage()): Partial<Record<MonitorKey, MonitorPrefValue>> {
  const raw = read<Record<string, unknown>>(s, STORAGE_KEYS.prefs) ?? {};
  const out: Partial<Record<MonitorKey, MonitorPrefValue>> = {};
  for (const k of MONITOR_KEYS) {
    const v = raw[k];
    if (typeof v === 'boolean' || typeof v === 'number' || typeof v === 'string') out[k] = v;
  }
  return out;
}

export function savePref(key: MonitorKey, value: MonitorPrefValue, s: Store | null = storage()) {
  write(s, STORAGE_KEYS.prefs, { ...loadPrefs(s), [key]: value });
}

export function loadMemory(s: Store | null = storage()): MonitorMemory | null {
  return read<MonitorMemory>(s, STORAGE_KEYS.memory);
}

export function loadPausedUntil(s: Store | null = storage()): number | null {
  const v = read<unknown>(s, STORAGE_KEYS.pausedUntil);
  return typeof v === 'number' ? v : null;
}

/** Keep what the monitor emitted. */
export function keepSnapshot(snap: MonitorSnapshot, s: Store | null = storage()) {
  write(s, STORAGE_KEYS.memory, snap.memory);
  write(s, STORAGE_KEYS.pausedUntil, snap.settings.paused_until ?? null);
}

/** The local clock's offset from UTC, in seconds (east positive). */
export function localOffsetSecs(d: Date = new Date()): number {
  return -d.getTimezoneOffset() * 60;
}

/** MonitorSettings from the stored preferences. `alert_days` is sent only when the user set
 * it here (otherwise `[proactive] alert_days` from config.toml applies). */
export function settingsFrom(
  prefs: Partial<Record<MonitorKey, MonitorPrefValue>>,
  pausedUntil: number | null,
  offsetSecs: number,
): MonitorSettings {
  const d = MONITOR_SETTING_DEFAULTS;
  const every = durationSecs(typeof prefs['app.monitor_every'] === 'string' ? prefs['app.monitor_every'] : undefined);
  const cooldown = durationSecs(typeof prefs['app.alert_cooldown'] === 'string' ? prefs['app.alert_cooldown'] : undefined);
  const growth = prefs['app.sudden_growth'] !== false;
  const days = prefs['app.alert_days'];
  return {
    alerts: true,
    interval_secs: every ?? d.interval_secs,
    cooldown_secs: cooldown ?? d.cooldown_secs,
    min_severity: d.min_severity,
    muted_kinds: growth ? [] : [...GROWTH_KINDS],
    muted_subjects: [],
    alert_days: typeof days === 'number' ? days : null,
    paused_until: pausedUntil,
    digest: { ...d.digest, enabled: prefs['app.weekly_digest'] === true, utc_offset_secs: offsetSecs },
  };
}

/** The settings to send now, from local storage. */
export function currentSettings(s: Store | null = storage(), offsetSecs = localOffsetSecs()): MonitorSettings {
  return settingsFrom(loadPrefs(s), loadPausedUntil(s), offsetSecs);
}

/** Save one preference and send the new settings to the monitor. */
export async function saveMonitorPref(control: MonitorControl, key: MonitorKey, value: MonitorPrefValue, s: Store | null = storage()) {
  savePref(key, value, s);
  await control.configure({ settings: currentSettings(s) });
}

/** Main window start: follow the monitor's snapshots, then send settings and saved memory. */
export async function startMonitorSync(control: MonitorControl, s: Store | null = storage()): Promise<() => void> {
  const stop = await control.onSnapshot((snap) => keepSnapshot(snap, s));
  await control.configure({ settings: currentSettings(s), memory: loadMemory(s) });
  return stop;
}
