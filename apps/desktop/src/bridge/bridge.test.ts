import { describe, expect, it } from 'vitest';
import { insideTauri, selectBridge, wantsFirstRun } from './index';
import { createMockBridge, MOCK_DRIVES } from './mock';

describe('bridge selection', () => {
  it('uses the mock in a plain browser', () => {
    expect(insideTauri({})).toBe(false);
    expect(selectBridge({}).kind).toBe('mock');
  });
  it('uses Tauri inside the app', () => {
    expect(insideTauri({ __TAURI_INTERNALS__: {} })).toBe(true);
    expect(insideTauri({ __TAURI__: {} })).toBe(true);
    expect(selectBridge({ __TAURI_INTERNALS__: {} }).kind).toBe('tauri');
  });
  it('asks for a never-scanned machine only with ?scenario=first-run', () => {
    expect(wantsFirstRun({ location: { search: '', hash: '#/?scenario=first-run' } })).toBe(true);
    expect(wantsFirstRun({ location: { search: '?scenario=first-run', hash: '' } })).toBe(true);
    expect(wantsFirstRun({ location: { search: '', hash: '#/?scenario=design' } })).toBe(false);
    expect(wantsFirstRun({})).toBe(false);
  });
});

describe('mock bridge, first run', () => {
  it('reports no scan until the first complete scan', async () => {
    const b = createMockBridge({ firstRun: true });
    expect((await b.status()).state).toBe('no_scan');
    expect((await b.status()).last_scan ?? null).toBeNull();
    const ac = new AbortController();
    ac.abort();
    await b.scan({}, { signal: ac.signal });
    expect((await b.status()).state).toBe('no_scan');
    await b.scan({});
    expect((await b.status()).state).toBe('ok');
  });
  it('is a scanned machine by default', async () => {
    expect((await createMockBridge().status()).state).toBe('ok');
  });
});

describe('mock bridge', () => {
  it('answers app info', async () => {
    const info = await createMockBridge().appInfo();
    expect(info.bridge).toBe('mock');
  });

  it('lists the home drive first and returns copies', async () => {
    const b = createMockBridge();
    const drives = await b.drives();
    expect(drives[0]?.holds_home).toBe(true);
    drives[0]!.label = 'changed';
    expect((await b.drives())[0]?.label).toBe(MOCK_DRIVES[0]?.label);
  });

  it('never reports an unknown size as 0', () => {
    for (const d of MOCK_DRIVES) {
      expect(d.total_bytes == null || d.total_bytes > 0).toBe(true);
    }
  });

  it('exposes no mutating call (SAFE-10)', () => {
    const names = Object.keys(createMockBridge());
    for (const n of names) {
      expect(n).not.toMatch(/delete|remove|clean|trash|move|rename|write|quarantine|execute/i);
    }
  });
});
