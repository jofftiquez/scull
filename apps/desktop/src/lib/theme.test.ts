// Theme preference (P5-UI-01): system by default, a saved override, applied to <html>.
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { THEME_KEY, THEME_OPTIONS, applyTheme, initTheme, isThemePref, readThemePref, resolvedTheme, useTheme } from './theme';

const root = () => document.documentElement;

beforeEach(() => {
  localStorage.clear();
  delete root().dataset.theme;
});
afterEach(() => {
  localStorage.clear();
  delete root().dataset.theme;
});

describe('theme preference', () => {
  it('offers System, Light and Dark, System first (the default)', () => {
    expect(THEME_OPTIONS.map(([v, l]) => `${v}:${l}`)).toEqual(['system:System', 'light:Light', 'dark:Dark']);
    expect(['system', 'light', 'dark'].every(isThemePref)).toBe(true);
    expect(isThemePref('sepia')).toBe(false);
    expect(isThemePref(null)).toBe(false);
  });

  it('follows the system when nothing (or something unknown) is saved', () => {
    expect(readThemePref()).toBe('system');
    localStorage.setItem(THEME_KEY, 'sepia');
    expect(readThemePref()).toBe('system');
    localStorage.setItem(THEME_KEY, 'dark');
    expect(readThemePref()).toBe('dark');
  });

  it('falls back to the system when storage is missing or throws', () => {
    expect(readThemePref(null)).toBe('system');
    const broken = {
      getItem() {
        throw new Error('blocked');
      },
    } as unknown as Storage;
    expect(readThemePref(broken)).toBe('system');
  });

  it('a forced theme sets html[data-theme]; system removes it', () => {
    applyTheme('dark');
    expect(root().dataset.theme).toBe('dark');
    applyTheme('light');
    expect(root().dataset.theme).toBe('light');
    applyTheme('system');
    expect(root().dataset.theme).toBeUndefined();
  });

  it('resolves system to the OS preference, a forced theme to itself', () => {
    expect(resolvedTheme('system', () => true)).toBe('dark');
    expect(resolvedTheme('system', () => false)).toBe('light');
    expect(resolvedTheme('light', () => true)).toBe('light');
    expect(resolvedTheme('dark', () => false)).toBe('dark');
  });

  it('setTheme saves, applies and updates the shared preference; system clears the saved value', () => {
    const a = useTheme();
    const b = useTheme();
    a.setTheme('dark');
    expect(localStorage.getItem(THEME_KEY)).toBe('dark');
    expect(root().dataset.theme).toBe('dark');
    expect(b.pref.value).toBe('dark');
    a.setTheme('system');
    expect(localStorage.getItem(THEME_KEY)).toBeNull();
    expect(root().dataset.theme).toBeUndefined();
    expect(b.pref.value).toBe('system');
  });

  it('initTheme applies the saved theme before mount and follows other windows (storage event)', () => {
    localStorage.setItem(THEME_KEY, 'light');
    initTheme();
    expect(root().dataset.theme).toBe('light');
    expect(useTheme().pref.value).toBe('light');
    // The menu bar widget (another window of the app) picks Dark.
    localStorage.setItem(THEME_KEY, 'dark');
    window.dispatchEvent(new StorageEvent('storage', { key: THEME_KEY }));
    expect(root().dataset.theme).toBe('dark');
    expect(useTheme().pref.value).toBe('dark');
    // Unrelated keys are ignored; clearing storage (key null) goes back to the system.
    localStorage.setItem(THEME_KEY, 'light');
    window.dispatchEvent(new StorageEvent('storage', { key: 'other' }));
    expect(root().dataset.theme).toBe('dark');
    localStorage.clear();
    window.dispatchEvent(new StorageEvent('storage', { key: null }));
    expect(root().dataset.theme).toBeUndefined();
  });
});
