// Menu bar / tray widget (design 11 / 11b, P4-UI-01): idle, cleaning and done, "Check now", and
// the buttons that open the main window. The widget is its own page (menubar.html) in a
// 380 × 660 window. On the mock bridge Quick Clean is simulated (nothing exists to delete);
// inside Tauri the same button is refused (covered by the unit tests).
import { expect, test } from './fixtures';
import type { Page } from '@playwright/test';

test.use({ viewport: { width: 380, height: 660 } });

async function openWidget(page: Page, query = ''): Promise<void> {
  await page.goto(`/menubar.html${query}`);
  await expect(page.getByTestId('menubar')).toBeVisible();
  const tauri = await page.evaluate(() => {
    const g = globalThis as { __TAURI__?: unknown; __TAURI_INTERNALS__?: unknown };
    return g.__TAURI__ !== undefined || g.__TAURI_INTERNALS__ !== undefined;
  });
  expect(tauri, 'E2E runs the browser build with the mock bridge').toBe(false);
  await expect(page.getByRole('main')).toHaveAttribute('aria-busy', 'false');
}

test('idle, cleaning and done (design 11 / 11b)', async ({ page, shot }) => {
  await openWidget(page, '?demo=design');
  const w = page.getByTestId('menubar');
  await expect(w.getByTestId('free')).toHaveText('41.2 GiB');
  await expect(w.getByTestId('used-pct')).toHaveText('92%');
  await expect(w.getByTestId('days-badge')).toHaveText('9 days left');
  await expect(w.getByTestId('forecast')).toHaveText('full in about 9 days');
  await expect(w.getByTestId('reclaim-bytes')).toHaveText('38.6 GiB');
  await expect(w.getByTestId('tools')).toHaveText('npm · pnpm · Bun · Yarn · re-downloadable');
  await expect(w.getByTestId('cache-bytes')).toHaveText('14.9 GiB');
  await expect(w.getByTestId('alert')).toHaveCount(2);
  await expect(w.getByTestId('alert').first()).toContainText('22 agent worktrees past their TTL');
  await expect(w.getByTestId('check')).toHaveText('Checked 4 min ago');
  const button = w.getByTestId('quick-clean');
  await expect(button).toHaveText('Quick Clean · 14.9 GiB');
  await shot('menubar-1-idle');

  await button.click();
  await expect(button).toContainText('Cleaning…');
  await expect(button).toBeDisabled();
  await shot('menubar-2-cleaning', { settle: false });

  await expect(button).toHaveText('+14.9 GiB freed ✓');
  await expect(w.getByTestId('free')).toHaveText('56.1 GiB');
  await expect(w.getByTestId('used-pct')).toHaveText('89%');
  await expect(w.getByTestId('days-badge')).toHaveText('13 days left');
  await expect(w.getByTestId('cache-bytes')).toHaveText('0 B');
  await expect(w.getByTestId('simulated')).toHaveText('simulated · mock bridge, nothing deleted');
  await shot('menubar-3-done');

  // Done -> back to idle (design: the button resets).
  await button.click();
  await expect(button).toHaveText('Quick Clean · 14.9 GiB');
  await expect(w.getByTestId('free')).toHaveText('41.2 GiB');
});

test('Check now re-reads, and the buttons open the main window', async ({ page, shot }) => {
  await openWidget(page, '?demo=design');
  const check = page.getByTestId('check');
  await check.click();
  await expect(check).toHaveText('Checking…');
  await expect(check).toHaveText('Checked just now');
  await shot('menubar-4-checked');

  // An alert opens its screen in the main app (in the browser: the app's page on that route).
  await page.getByTestId('alert').first().click();
  await expect(page).toHaveURL(/\/index\.html#\/worktrees$/);
  await expect(page.getByRole('navigation').first()).toBeVisible();

  await openWidget(page, '?demo=design');
  await page.getByTestId('review').click();
  await expect(page).toHaveURL(/\/index\.html#\/cleanup$/);
});

test('recorded data: no plan, no alerts, honest empty states', async ({ page, shot }) => {
  await openWidget(page);
  const w = page.getByTestId('menubar');
  // Free space comes from the drives call (the home drive).
  await expect(w.getByTestId('free')).toHaveText('41.2 GiB');
  await expect(w.getByTestId('reclaim')).toContainText('Nothing safe to reclaim yet');
  await expect(w.getByTestId('review')).toHaveCount(0);
  await expect(w.getByTestId('no-alerts')).toHaveText('No alerts');
  await shot('menubar-5-recorded');
});
