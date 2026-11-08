// Keyboard only (WCAG 2.1.1): skip link → wheel (arrow keys, Enter, Shift+Enter) → sidebar →
// Cleanup Review → confirm → back to the overview, without the mouse.
import { expect, isFocused, open, tabTo, test } from './fixtures';

test('select, add to cleanup and confirm the plan using only the keyboard', async ({ page, shot }) => {
  await open(page, '/?scenario=design');
  await expect(page.getByTestId('rescan')).toHaveText(/Scan/);
  await expect(page.getByTestId('rescan')).toBeEnabled();

  // The first Tab stop skips the navigation.
  await page.keyboard.press('Tab');
  const skip = page.getByTestId('skip-link');
  await expect(skip).toBeFocused();
  await expect(skip).toBeVisible();
  await page.keyboard.press('Enter');
  await expect(page.locator('#content')).toBeFocused();

  // Into the wheel (one Tab stop), then arrows to the first Regenerable item.
  await tabTo(page, page.locator('[data-seg][tabindex="0"]'));
  const focusedTier = () => page.evaluate(() => (document.activeElement as HTMLElement | null)?.dataset.tier ?? null);
  for (let i = 0; i < 40 && (await focusedTier()) !== 'S0'; i++) await page.keyboard.press('ArrowRight');
  expect(await focusedTier()).toBe('S0');
  await page.keyboard.press('Enter');
  const detail = page.getByTestId('selected');
  await expect(detail.getByTestId('badge')).toHaveText('Regenerable');
  await page.keyboard.press('Shift+Enter');
  await expect(page.getByTestId('announce')).toContainText('to cleanup');
  await expect(page.getByTestId('basket')).toHaveText('1 item marked for cleanup');
  await shot('keyboard-1-wheel-selected');

  // Back to the sidebar and into Cleanup Review; focus lands on the new screen's main.
  const nav = page.getByRole('navigation', { name: 'Primary' }).getByRole('link', { name: /Cleanup Review/ });
  await tabTo(page, nav, { back: true });
  await page.keyboard.press('Enter');
  await expect(page).toHaveURL(/#\/cleanup$/);
  await expect(page.locator('#content')).toBeFocused();

  const confirm = page.getByTestId('confirm');
  await expect(confirm).toBeEnabled();
  await tabTo(page, confirm, { max: 120 });
  await shot('keyboard-2-confirm-focused');
  await page.keyboard.press('Enter');
  await expect(page.getByRole('dialog')).toBeVisible();
  await tabTo(page, page.getByTestId('confirm-action'));
  await page.keyboard.press('Enter');
  const done = page.getByTestId('done');
  await expect(done).toBeVisible();

  const back = page.getByTestId('main-footer').getByRole('link', { name: 'Overview', exact: true });
  await expect(back).toBeVisible();
  // The task indicator receives focus while the job runs; return to the footer with Tab.
  await tabTo(page, back, { max: 120 });
  expect(await isFocused(back)).toBe(true);
  await shot('keyboard-3-done');
  await page.keyboard.press('Enter');
  await expect(page).toHaveURL(/#\/$/);
});
