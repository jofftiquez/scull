import { describe, expect, it } from 'vitest';
import { insideTauri, selectBridge } from './index';
import { createMockBridge, MOCK_DATA } from './mock';

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
});

describe('mock bridge', () => {
  it('returns copies, never its own data', async () => {
    const b = createMockBridge();
    const s = await b.settings();
    s.config.scan = { roots: ['/changed'] };
    expect((await b.settings()).config.scan?.roots).toEqual(MOCK_DATA.settings.config.scan?.roots);
  });

  it('exposes no mutating call (SAFE-10)', () => {
    const names = Object.keys(createMockBridge());
    for (const n of names) {
      expect(n).not.toMatch(/delete|remove|clean|trash|move|rename|write|quarantine|execute/i);
    }
  });
});
