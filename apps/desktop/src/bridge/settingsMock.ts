// The design's settings (design 10), in the REAL `SettingsView` shape, for the mock bridge:
// a config file with the design's scan folders, MCP turned on, one folder protected by the user,
// everything else at its built-in default. Like MOCK_DRIVES it stands in for a configured
// machine; the recorded fixtures come from a session with no config file.
import type { Origin, SettingsView } from './types';

const HOME = '/Users/ada';
const SUPPORT = `${HOME}/Library/Application Support/scull`;
const MiB = 1024 ** 2;

const FIELDS = [
  'scan.roots',
  'scan.exclude',
  'scan.protect',
  'scan.min_tier',
  'scan.activity_window',
  'scan.threads',
  'scan.low_resource',
  'rules.enabled',
  'mcp.enabled',
  'mcp.destructive',
  'mcp.redact_home',
  'mcp.hash_names',
  'mcp.max_results',
  'forge.enabled',
  'output.color',
  'output.units',
] as const;

const FROM_FILE = new Set(['scan.roots', 'scan.protect', 'mcp.enabled']);

export function designSettings(): SettingsView {
  const origins: Record<string, Origin[]> = {};
  for (const f of FIELDS) origins[f] = [FROM_FILE.has(f) ? { source: 'user_config' } : { source: 'default' }];
  return {
    config_file: `${SUPPORT}/config.toml`,
    config_state: 'present',
    config_error: null,
    config: {
      scan: {
        roots: ['~/code', '~/work', '~/.cache'],
        exclude: [],
        protect: ['~/code/payments'],
        min_tier: {},
        activity_window: '24h',
        threads: 0,
        low_resource: false,
      },
      rules: { enabled: [] },
      mcp: { enabled: true, destructive: false, redact_home: true, hash_names: false, max_results: 1000 },
      forge: { enabled: false },
      output: { color: 'auto', units: 'binary' },
    },
    origins,
    builtin_protected: [`${HOME}/.ssh`, `${HOME}/.gnupg`, `${HOME}/.aws`, `${HOME}/.kube`, `${HOME}/.password-store`, `${HOME}/Library`],
    data: {
      config_dir: SUPPORT,
      data_dir: SUPPORT,
      database: {
        path: `${SUPPORT}/scull.db`,
        state: 'ok',
        schema_version: 2,
        supported_schema_version: 2,
        detail: null,
      },
      files: [
        { path: `${SUPPORT}/scull.db`, kind: 'database', bytes: Math.round(196 * MiB) },
        { path: `${SUPPORT}/scull.db-wal`, kind: 'wal', bytes: Math.round(12 * MiB) },
        { path: `${SUPPORT}/scull.db-shm`, kind: 'shm', bytes: 32768 },
        { path: `${SUPPORT}/scull.db.bak-v1`, kind: 'backup', bytes: Math.round(4 * MiB) },
      ],
      footprint_bytes: Math.round(212 * MiB) + 32768,
    },
  };
}
