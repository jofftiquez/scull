// Dark theme (P5-UI-01): the app follows the system's color scheme, Settings > General >
// Appearance can force light or dark, the choice survives a reload and the menu bar widget
// follows it. Key screens are rendered in dark with their end states visible.
import { expect, open, settle, test } from './fixtures';
import type { Page } from '@playwright/test';

const DARK_GROUND = 'rgb(18, 17, 16)'; // tokens.dark.color.ground #121110
const LIGHT_GROUND = 'rgb(247, 245, 242)'; // tokens.color.ground #F7F5F2

const ground = (page: Page) => page.evaluate(() => getComputedStyle(document.body).backgroundColor);
const scheme = (page: Page) => page.evaluate(() => getComputedStyle(document.documentElement).colorScheme);
const cssVar = (page: Page, name: string) =>
  page.evaluate((n) => getComputedStyle(document.documentElement).getPropertyValue(n).trim(), name);

const SCREENS: [string, string, string][] = [
  ['overview', '/', '[data-testid="wheel"]'],
  ['cleanup', '/cleanup', '[data-testid="confirm"]'],
  ['worktrees', '/worktrees', 'h1'],
  ['insights', '/insights?demo=design', '[data-testid="forecast-chart"]'],
  ['projects', '/projects?demo=design', 'h1'],
  ['explorer', '/explorer', 'svg .tile'],
  ['caches', '/caches?demo=design', '[data-testid="hero"]'],
  ['history', '/history', '[data-testid="log"]'],
  ['settings', '/settings', '[data-testid="appearance"]'],
];

test.describe('system dark mode', () => {
  test.use({ colorScheme: 'dark' });

  for (const [name, path, ready] of SCREENS) {
    test(`${name} renders in dark`, async ({ page, shot }) => {
      await open(page, path);
      await expect(page.locator(ready).first()).toBeVisible();
      expect(await ground(page)).toBe(DARK_GROUND);
      expect(await scheme(page)).toBe('dark');
      // Paper and ink swap: the selected nav item is the light pill with dark text.
      const current = page.getByRole('navigation', { name: 'Primary' }).locator('[aria-current="page"]');
      if (await current.count()) {
        expect(await current.first().evaluate((el) => getComputedStyle(el).backgroundColor)).toBe('rgb(244, 241, 236)');
      }
      // The protected tier uses its lighter dark-theme hatch.
      expect((await cssVar(page, '--color-tier-s3')).toLowerCase()).toBe('#7f6a9b');
      await shot(`dark-${name}`);
    });
  }

  test('the menu bar widget renders in dark', async ({ page, shot }) => {
    await page.setViewportSize({ width: 400, height: 640 });
    await page.goto('/menubar.html');
    await expect(page.getByTestId('quick-clean').or(page.locator('#app > *')).first()).toBeVisible();
    expect(await ground(page)).toBe(DARK_GROUND);
    await settle(page);
    await shot('dark-menubar');
  });

  test('Settings can force light, which survives a reload and reaches the menu bar widget', async ({ page, context }) => {
    await open(page, '/settings');
    const group = page.getByRole('group', { name: 'Theme' });
    await expect(group.getByRole('button', { name: 'System' })).toHaveAttribute('aria-pressed', 'true');
    expect(await ground(page)).toBe(DARK_GROUND);

    // The menu bar widget is open in another window of the app.
    const widget = await context.newPage();
    await widget.goto('/menubar.html');
    await expect(widget.locator('#app > *').first()).toBeVisible();
    expect(await ground(widget)).toBe(DARK_GROUND);

    await group.getByRole('button', { name: 'Light' }).click();
    await expect(group.getByRole('button', { name: 'Light' })).toHaveAttribute('aria-pressed', 'true');
    await expect(page.locator('html')).toHaveAttribute('data-theme', 'light');
    await expect.poll(() => ground(page)).toBe(LIGHT_GROUND);
    expect(await scheme(page)).toBe('light');
    await expect.poll(() => ground(widget)).toBe(LIGHT_GROUND);

    await page.reload();
    await expect(page.getByRole('group', { name: 'Theme' }).getByRole('button', { name: 'Light' })).toHaveAttribute('aria-pressed', 'true');
    expect(await ground(page)).toBe(LIGHT_GROUND);

    // Back to the system: dark again, and the saved choice is gone.
    await page.getByRole('group', { name: 'Theme' }).getByRole('button', { name: 'System' }).click();
    await expect(page.locator('html')).not.toHaveAttribute('data-theme', /./);
    await expect.poll(() => ground(page)).toBe(DARK_GROUND);
    expect(await page.evaluate(() => localStorage.getItem('scull.theme'))).toBeNull();
    await widget.close();
  });
});

test.describe('system light mode', () => {
  test.use({ colorScheme: 'light' });

  test('light by default; Settings can force dark', async ({ page, shot }) => {
    await open(page, '/settings');
    expect(await ground(page)).toBe(LIGHT_GROUND);
    expect(await scheme(page)).toBe('light');
    const group = page.getByRole('group', { name: 'Theme' });
    await group.getByRole('button', { name: 'Dark' }).click();
    await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark');
    await expect.poll(() => ground(page)).toBe(DARK_GROUND);
    expect(await scheme(page)).toBe('dark');
    await page.getByTestId('appearance').scrollIntoViewIfNeeded();
    await shot('dark-forced-settings-appearance', { show: page.getByTestId('appearance') });
  });
});
