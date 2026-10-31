// What the Settings screen shows (design 10), derived from the `settings` and `drives` bridge
// payloads. Pure functions, tested without rendering.
import type { Drive, Origin, ProjectsView, SettingsView } from '../../bridge';
import { formatBytes } from '../../lib/format';

export type TabId = 'general' | 'drives' | 'safety' | 'alerts' | 'agents' | 'data' | 'about';

export const TABS: readonly { id: TabId; label: string }[] = [
  { id: 'general', label: 'General' },
  { id: 'drives', label: 'Drives' },
  { id: 'safety', label: 'Safety' },
  { id: 'alerts', label: 'Alerts' },
  { id: 'agents', label: 'AI agents' },
  { id: 'data', label: 'Data & privacy' },
  { id: 'about', label: 'About' },
];

export function isTab(v: unknown): v is TabId {
  return typeof v === 'string' && TABS.some((t) => t.id === v);
}

/** Where a value came from, as shown next to it: `default`, `config.toml`, `env SCULL_X`. */
export function originLabel(origins: Origin[] | undefined): string {
  const o = origins?.[0];
  if (!o) return 'default';
  switch (o.source) {
    case 'user_config':
      return origins!.length > 1 ? 'config.toml + more' : 'config.toml';
    case 'env':
      return `env ${o.var}`;
    case 'cli':
      return 'command line';
    default:
      return 'default';
  }
}

/** Seconds of a config duration (`24h`, `30m`, `7d`, `90s`). */
export function durationSecs(s: string | undefined): number | null {
  const m = /^\s*(\d+)\s*([smhdw])\s*$/.exec(s ?? '');
  if (!m) return null;
  const mult = { s: 1, m: 60, h: 3600, d: 86400, w: 604800 }[m[2] as 's' | 'm' | 'h' | 'd' | 'w'];
  return Number(m[1]) * mult;
}

/** Segmented options: `[value, label]`. Values are what gets saved. */
export const ACTIVITY_WINDOWS: readonly [string, string][] = [
  ['1h', '1 hour'],
  ['24h', '24 hours'],
  ['168h', '7 days'],
];
export const CHECK_EVERY: readonly [string, string][] = [
  ['1h', '1 h'],
  ['6h', '6 h'],
  ['24h', '24 h'],
];
export const CAUTION: readonly [string, string][] = [
  ['very', 'Very'],
  ['balanced', 'Balanced'],
  ['expert', 'Expert'],
];
export const RETENTION: readonly [string, string][] = [
  ['3d', '3 days'],
  ['7d', '7 days'],
  ['30d', '30 days'],
];
export const HISTORY_KEEP: readonly [string, string][] = [
  ['3m', '3 months'],
  ['12m', '12 months'],
  ['forever', 'Forever'],
];

/** The option matching a configured activity window, by duration (`86400s` is `24h`). */
export function activityOption(window: string | undefined): string | null {
  const secs = durationSecs(window);
  return ACTIVITY_WINDOWS.find(([v]) => durationSecs(v) === secs)?.[0] ?? null;
}

/** App preferences without a config key yet, at their defaults (design 10). */
export const APP_DEFAULTS = {
  'app.check_every': '6h',
  'app.menu_bar': true,
  'app.open_at_login': true,
  'app.caution': 'balanced',
  'app.quarantine_retention': '7d',
  'app.type_to_confirm': true,
  'app.alert_days': 14,
  'app.sudden_growth': true,
  'app.weekly_digest': false,
  'app.history_keep': '12m',
  'app.check_updates': false,
} as const;

export type AppKey = keyof typeof APP_DEFAULTS;

/** `/Users/ada/code/x` → `~/code/x`. */
export function tilde(p: string): string {
  return p.replace(/^\/(?:Users|home)\/[^/]+(?=\/|$)/, '~').replace(/^[A-Za-z]:\\Users\\[^\\]+(?=\\|$)/, '~');
}

/** `32 projects` under a configured root, from the last scan (empty when unknown). */
export function rootNote(root: string, projects: ProjectsView | null): string {
  if (!projects || projects.projects.length === 0) return '';
  const r = tilde(root).replace(/[\\/]+$/, '');
  const n = projects.projects.filter((p) => {
    const t = tilde(p.root);
    return t === r || t.startsWith(`${r}/`) || t.startsWith(`${r}\\`);
  }).length;
  return n ? `${n} project${n === 1 ? '' : 's'}` : '';
}

export interface DriveRow {
  key: string;
  name: string;
  kind: string;
  sub: string;
  mode: string;
  on: boolean;
  locked: boolean;
}

const KIND_LABEL: Record<Drive['kind'], string> = {
  local: 'internal',
  removable: 'external',
  network: 'network',
  virtual: 'virtual',
  backup: 'Time Machine',
};

/** One row per drive: how scull treats it (D-22). Backup drives are locked off. */
export function driveRows(drives: readonly Drive[], overrides: ReadonlyMap<string, boolean> = new Map()): DriveRow[] {
  return drives.map((d) => {
    const locked = d.scan_policy === 'never';
    const base = d.scan_policy === 'scan';
    const on = locked ? false : (overrides.get(d.mount_point) ?? base);
    const size =
      d.total_bytes != null && d.free_bytes != null
        ? `${formatBytes(d.total_bytes)} · ${formatBytes(d.free_bytes)} free`
        : 'size unknown';
    const sub =
      d.kind === 'backup'
        ? 'Backup drives are never scanned or cleaned'
        : d.kind === 'network' || d.kind === 'virtual'
          ? 'Network drives are slow to scan; off unless you need it'
          : size;
    const mode = locked
      ? 'never'
      : d.scan_policy === 'off_by_default'
        ? 'off by default'
        : d.holds_home
          ? 'always'
          : 'when connected';
    return { key: d.mount_point, name: d.label || d.mount_point, kind: KIND_LABEL[d.kind], sub, mode, on, locked };
  });
}

/** The design's budgets (design 10, Alerts), shown with the mock bridge only: budgets arrive
 * with Insights (INS-02). */
export const DESIGN_BUDGETS: readonly { name: string; usedBytes: number; capBytes: number }[] = [
  { name: 'Hugging Face cache', usedBytes: 31.2 * 1024 ** 3, capBytes: 25 * 1024 ** 3 },
  { name: 'Docker', usedBytes: 18.0 * 1024 ** 3, capBytes: 80 * 1024 ** 3 },
  { name: 'All Rust target folders', usedBytes: 21.9 * 1024 ** 3, capBytes: 50 * 1024 ** 3 },
];

/** The design's agent clients (design 10c), shown with the mock bridge only: client detection
 * arrives with `scull agent install` (§17). */
export const DESIGN_CLIENTS: readonly { id: string; initials: string; name: string; connected: boolean; tools: string }[] = [
  { id: 'claude-code', initials: 'CC', name: 'Claude Code', connected: true, tools: 'read-only tools + plan preparation' },
  { id: 'codex', initials: 'CX', name: 'Codex', connected: true, tools: 'read-only tools' },
  { id: 'cursor', initials: 'CU', name: 'Cursor', connected: false, tools: 'read-only tools' },
];

/** Storage budget for scull's own data (INS-02 default). */
export const DATA_BUDGET_BYTES = 1024 ** 3;

const DB_STATE_TEXT: Record<SettingsView['data']['database']['state'], string> = {
  ok: 'readable',
  missing: 'not created yet (the first recorded scan creates it)',
  empty: 'empty',
  newer_schema: 'written by a newer scull',
  needs_migration: 'older schema (migrated by the next scan in a terminal)',
  not_scull: 'not a scull database',
  unreadable: 'unreadable',
};

export function databaseLine(v: SettingsView): string {
  const d = v.data.database;
  return d.detail ? `${DB_STATE_TEXT[d.state]}: ${d.detail}` : DB_STATE_TEXT[d.state];
}
