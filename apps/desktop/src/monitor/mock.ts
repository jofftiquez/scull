// The mock monitor: in-memory settings and memory for the browser build (development, tests,
// E2E). The mock bridge's `monitorStatus` and the mock MonitorControl share it, like the real
// bridge and commands share the tray's monitor. It never shows a notification: "sent" digests
// are recorded in `sent` so tests and the UI can see them.
import type { DigestView, MonitorConfig, MonitorMemory, MonitorSettings, MonitorSnapshot, MonitorStatus } from '../bridge/types';
import type { MonitorControl } from './control';

const HOUR = 3600;
const DAY = 24 * HOUR;
const MAX_PAUSE = 7 * DAY;

export const MONITOR_SETTING_DEFAULTS: Required<Omit<MonitorSettings, 'paused_until' | 'alert_days'>> & {
  paused_until: number | null;
  alert_days: number | null;
} = {
  alerts: true,
  interval_secs: HOUR,
  cooldown_secs: DAY,
  min_severity: 'warning',
  muted_kinds: [],
  muted_subjects: [],
  alert_days: null,
  paused_until: null,
  digest: { enabled: false, weekday: 0, hour: 9, utc_offset_secs: 0 },
};

/** The next weekly slot (weekday 0 = Monday, hour, local offset) strictly after `now`. */
export function nextSlot(now: number, weekday: number, hour: number, offset: number): number {
  const local = now + offset;
  const days = Math.floor(local / DAY);
  const wd = (((days + 3) % 7) + 7) % 7;
  let slot = (days - ((((wd - weekday) % 7) + 7) % 7)) * DAY + hour * HOUR;
  if (slot <= local) slot += 7 * DAY;
  return slot - offset;
}

export interface MockMonitor {
  control: MonitorControl;
  status(now?: number): MonitorStatus;
  /** Digests "sent" with Send now. */
  sent: DigestView[];
  /** Back to defaults (tests). */
  reset(): void;
  /** The digest the mock bridge answers (set by the bridge). */
  digest: () => Promise<DigestView>;
}

function createMockMonitor(): MockMonitor {
  let settings: MonitorSettings = structuredClone(MONITOR_SETTING_DEFAULTS);
  let memory: MonitorMemory = {};
  let configured = false;
  const listeners = new Set<(s: MonitorSnapshot) => void>();
  const nowSecs = () => Math.floor(Date.now() / 1000);
  const emit = () => {
    const snap = { settings: structuredClone(settings), memory: structuredClone(memory) };
    for (const l of listeners) l(snap);
  };
  const m: MockMonitor = {
    sent: [],
    digest: async () => ({ state: 'no_database', detail: null, digest: null }),
    status(now = nowSecs()) {
      const paused = (settings.paused_until ?? 0) > now;
      const d = settings.digest ?? {};
      return {
        settings: structuredClone(settings),
        configured,
        paused,
        last_check: memory.last_check ?? null,
        next_check: Math.max(memory.last_check != null ? memory.last_check + (settings.interval_secs ?? HOUR) : now, paused ? settings.paused_until! : 0),
        last_digest: memory.last_digest ?? null,
        next_digest: d.enabled ? nextSlot(now, d.weekday ?? 0, d.hour ?? 9, d.utc_offset_secs ?? 0) : null,
        notified: Object.keys(memory.notified ?? {}).length,
      };
    },
    reset() {
      settings = structuredClone(MONITOR_SETTING_DEFAULTS);
      memory = {};
      configured = false;
      m.sent = [];
      listeners.clear();
    },
    control: {
      kind: 'mock',
      async configure(config: MonitorConfig) {
        settings = { ...structuredClone(MONITOR_SETTING_DEFAULTS), ...structuredClone(config.settings ?? {}) };
        if (!configured && config.memory) memory = structuredClone(config.memory);
        configured = true;
        return m.status();
      },
      async pause(secs: number | null) {
        const now = nowSecs();
        settings = { ...settings, paused_until: secs == null ? null : now + Math.min(Math.max(0, secs), MAX_PAUSE) };
        emit();
        return m.status(now);
      },
      async sendDigestNow() {
        const d = await m.digest();
        m.sent.push(d);
        return d;
      },
      async onSnapshot(handler) {
        listeners.add(handler);
        return () => listeners.delete(handler);
      },
    },
  };
  return m;
}

/** The one mock monitor of this page. */
export const mockMonitor: MockMonitor = createMockMonitor();
