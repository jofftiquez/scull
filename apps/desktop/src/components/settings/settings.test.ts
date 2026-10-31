import { describe, expect, it } from 'vitest';
import { createMockBridge, MOCK_DRIVES } from '../../bridge/mock';
import { designSettings } from '../../bridge/settingsMock';
import type { ProjectsView } from '../../bridge';
import { createMockSettingsWriter, createPendingSettingsWriter, settingsWriterFor } from './bridge';
import { activityOption, databaseLine, driveRows, durationSecs, isTab, originLabel, rootNote, tilde } from './model';

describe('settings model', () => {
  it('labels origins like `scull config show`', () => {
    expect(originLabel([{ source: 'default' }])).toBe('default');
    expect(originLabel([{ source: 'user_config' }])).toBe('config.toml');
    expect(originLabel([{ source: 'env', var: 'SCULL_ROOTS' }])).toBe('env SCULL_ROOTS');
    expect(originLabel([{ source: 'user_config' }, { source: 'env', var: 'SCULL_PROTECT' }])).toBe('config.toml + more');
    expect(originLabel(undefined)).toBe('default');
  });

  it('matches activity windows by duration, not spelling', () => {
    expect(durationSecs('24h')).toBe(86400);
    expect(durationSecs('1d')).toBe(86400);
    expect(durationSecs('nope')).toBeNull();
    expect(activityOption('24h')).toBe('24h');
    expect(activityOption('1440m')).toBe('24h');
    expect(activityOption('7d')).toBe('168h');
    expect(activityOption('2h')).toBeNull();
  });

  it('treats each drive per D-22: backup locked off, network off by default', () => {
    const rows = driveRows([
      ...MOCK_DRIVES,
      { mount_point: '/Volumes/nas', label: 'studio-nas', kind: 'network', scan_policy: 'off_by_default', holds_home: false, total_bytes: null, free_bytes: null },
    ]);
    expect(rows.map((r) => [r.name, r.kind, r.mode, r.on, r.locked])).toEqual([
      ['Macintosh HD', 'internal', 'always', true, false],
      ['Dev SSD', 'external', 'when connected', true, false],
      ['Backup (Time Machine)', 'Time Machine', 'never', false, true],
      ['studio-nas', 'network', 'off by default', false, false],
    ]);
    expect(rows[0]!.sub).toBe('494.0 GiB · 41.2 GiB free');
    // A backup drive stays off whatever the override says.
    expect(driveRows(MOCK_DRIVES, new Map([['/Volumes/Backup', true]]))[2]!.on).toBe(false);
    expect(driveRows(MOCK_DRIVES, new Map([['/', false]]))[0]!.on).toBe(false);
  });

  it('counts projects under a root from the last scan', () => {
    const view = { projects: [{ root: '/Users/ada/code/a' }, { root: '/Users/ada/code/b' }, { root: '/Users/ada/work/c' }] } as unknown as ProjectsView;
    expect(rootNote('~/code', view)).toBe('2 projects');
    expect(rootNote('~/work/', view)).toBe('1 project');
    expect(rootNote('~/.cache', view)).toBe('');
    expect(rootNote('~/code', null)).toBe('');
    expect(tilde('/home/ada/x')).toBe('~/x');
  });

  it('describes the database state', () => {
    const v = designSettings();
    expect(databaseLine(v)).toBe('readable');
    v.data.database = { ...v.data.database, state: 'newer_schema', detail: 'schema version 9' };
    expect(databaseLine(v)).toBe('written by a newer scull: schema version 9');
    expect(isTab('agents')).toBe(true);
    expect(isTab('nope')).toBe(false);
  });

  it('the design defaults follow PRD §13: MCP destructive off, home redaction on', () => {
    const v = designSettings();
    expect(v.config.mcp?.destructive).toBe(false);
    expect(v.config.mcp?.redact_home).toBe(true);
  });
});

describe('settings writer stub (screen-local, never on ScullBridge)', () => {
  it('the mock keeps values in memory and touches nothing', async () => {
    const w = createMockSettingsWriter();
    await w.save('mcp.hash_names', true);
    expect(w.values.get('mcp.hash_names')).toBe(true);
    expect(w.calls).toEqual(['save mcp.hash_names=true']);
  });

  it('mcp.destructive is file-only, even in the mock', async () => {
    await expect(createMockSettingsWriter().save('mcp.destructive', true)).rejects.toMatchObject({ kind: 'safety_blocked' });
    await expect(createPendingSettingsWriter().save('mcp.destructive', true)).rejects.toMatchObject({ kind: 'safety_blocked' });
  });

  it('inside Tauri every write refuses until the core config command exists', async () => {
    const w = createPendingSettingsWriter();
    for (const p of [w.save('scan.roots', []), w.connectAgent('x'), w.disconnectAgent('x'), w.exportData(), w.deleteHistory()]) {
      await expect(p).rejects.toMatchObject({ kind: 'unsupported_platform' });
    }
    expect(settingsWriterFor({ ...createMockBridge(), kind: 'tauri' }).kind).toBe('pending');
    expect(settingsWriterFor(createMockBridge()).kind).toBe('mock');
  });

  it('ScullBridge stays read-only: no write on it', () => {
    const keys = Object.keys(createMockBridge());
    expect(keys.some((k) => /^(save|write|set[A-Z]|connect|disconnect|export|delete)/.test(k))).toBe(false);
    expect(keys).toContain('settings');
  });
});
