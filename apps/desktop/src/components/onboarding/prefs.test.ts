import { beforeEach, describe, expect, it } from 'vitest';
import { MOCK_DETECTION } from './bridge';
import { canContinue, defaultAnswers } from './model';
import { loadSetup, saveSetup, savedScanParams, type SavedSetup } from './prefs';

const setup = (): SavedSetup => ({ version: 1, home: '/Users/ada', answers: { ...defaultAnswers(MOCK_DETECTION), connectAgents: false } });
beforeEach(() => localStorage.clear());

describe('survey progression', () => {
  it('requires a build choice and a confirmed folder on their respective steps', () => {
    const a = setup().answers;
    a.builds = [];
    expect(canContinue(1, a, '/Users/ada')).toBe(false);
    a.builds = ['other'];
    expect(canContinue(1, a, '/Users/ada')).toBe(true);
    a.roots = ['/Users/ada'];
    expect(canContinue(2, a, '/Users/ada')).toBe(false);
    a.confirmedBroad = ['/Users/ada'];
    expect(canContinue(2, a, '/Users/ada')).toBe(true);
    expect(canContinue(3, a, '/Users/ada')).toBe(true);
    expect(canContinue(4, a, '/Users/ada')).toBe(true);
    expect(canContinue(5, a, '/Users/ada')).toBe(true);
  });
});

describe('local setup', () => {
  it('remembers selected folders across reads without connecting clients', () => {
    expect(saveSetup(setup())).toEqual({ saved: true });
    expect(loadSetup()).toEqual(setup());
    expect(savedScanParams(loadSetup())).toEqual({ roots: ['/Users/ada/code', '/Users/ada/work'], allow_broad_roots: false });
  });

  it.each([
    '{bad json',
    JSON.stringify({ ...setup(), version: 2 }),
    JSON.stringify({ ...setup(), home: '' }),
    JSON.stringify({ ...setup(), answers: { ...setup().answers, builds: ['unknown'] } }),
    JSON.stringify({ ...setup(), answers: { ...setup().answers, agents: 'unknown' } }),
    JSON.stringify({ ...setup(), answers: { ...setup().answers, priority: 'unknown' } }),
    JSON.stringify({ ...setup(), answers: { ...setup().answers, caution: 'unknown' } }),
    JSON.stringify({ ...setup(), answers: { ...setup().answers, connectAgents: true } }),
    JSON.stringify({ ...setup(), answers: { ...setup().answers, roots: ['relative'] } }),
    JSON.stringify({ ...setup(), answers: { ...setup().answers, roots: ['/Users/ada/../bob'] } }),
    JSON.stringify({ ...setup(), answers: { ...setup().answers, confirmedBroad: ['relative'] } }),
  ])('ignores invalid storage rather than inventing scan roots (%s)', (raw) => {
    localStorage.setItem('scull.onboarding.setup', raw);
    expect(loadSetup()).toBeNull();
    expect(savedScanParams(loadSetup())).toBeNull();
  });

  it('keeps confirmed broad paths separate from selected paths', () => {
    const s = setup();
    s.answers.roots = ['/Users/ada', '/Users/ada/code'];
    s.answers.confirmedBroad = [];
    expect(savedScanParams(s)).toEqual({ roots: ['/Users/ada/code'], allow_broad_roots: false });
    s.answers.confirmedBroad = ['/Users/ada', '/unselected'];
    expect(saveSetup(s).saved).toBe(true);
    expect(loadSetup()!.answers.confirmedBroad).toEqual(['/Users/ada']);
    expect(savedScanParams(loadSetup())).toEqual({ roots: ['/Users/ada', '/Users/ada/code'], allow_broad_roots: true });
  });

  it('reports unavailable storage and allows callers to offer a one-time scan', () => {
    const store = { getItem: () => { throw new Error('unavailable'); }, setItem: () => { throw new Error('full'); } };
    expect(loadSetup(store)).toBeNull();
    expect(saveSetup(setup(), store)).toMatchObject({ saved: false, reason: expect.any(String) });
    expect(saveSetup(setup(), null).saved).toBe(false);
    expect(savedScanParams(setup())!.roots).toEqual(['/Users/ada/code', '/Users/ada/work']);
  });
});
