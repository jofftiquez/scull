// Overview tier filter + selection → add to cleanup → Cleanup Review → confirm → done (mock).
// Design 01 (wheel) and 03 / 03b (review). The mock "cleans" with timers; nothing is deleted.
import type { Page } from '@playwright/test';
import { clickSegment, expect, open, test } from './fixtures';

/** The Overview with the design's machine, once its first scan (and the radar sweep) is done. */
async function overviewReady(page: Page) {
  await open(page, '/?scenario=design');
  const rescan = page.getByTestId('rescan');
  await expect(rescan).toHaveText(/Rescan/);
  await expect(rescan).toBeEnabled();
  await expect(page.locator('[data-kind="item"]')).toHaveCount(16);
}

const opacity = (page: Page, tier: string) =>
  page.locator(`[data-kind="item"][data-tier="${tier}"]`).first().getAttribute('opacity').then(Number);

test('filter the wheel, add items to cleanup, review and confirm the plan', async ({ page, shot }) => {
  await overviewReady(page);
  await shot('cleanup-1-overview');

  // Tier filter: only Regenerable (S0) stays at full strength.
  const tools = page.getByRole('toolbar', { name: 'Wheel tools' });
  await tools.getByRole('button', { name: 'Regenerable' }).click();
  await expect(tools.getByRole('button', { name: 'Regenerable' })).toHaveAttribute('aria-pressed', 'true');
  await expect(tools.getByRole('button', { name: 'All' })).toHaveAttribute('aria-pressed', 'false');
  await expect.poll(() => opacity(page, 'S1')).toBeLessThan(0.5);
  expect(await opacity(page, 'S0')).toBe(1);

  // Select an S0 segment with the mouse: the detail card explains it and offers the action.
  const s0 = page.locator('[data-kind="item"][data-tier="S0"]');
  await clickSegment(page, s0.first());
  await expect(s0.first()).toHaveAttribute('aria-pressed', 'true');
  const detail = page.getByTestId('selected');
  await expect(detail.getByTestId('badge')).toHaveText('Regenerable');
  await expect(detail.getByTestId('detail-action')).toHaveText('Add to cleanup');
  await shot('cleanup-2-filtered-and-selected');

  await detail.getByTestId('detail-action').click();
  await expect(page.getByTestId('announce')).toContainText('to cleanup');
  await expect(page.getByTestId('basket')).toHaveText('1 item marked for cleanup');

  // Shift-click adds another segment without changing the selection.
  await clickSegment(page, s0.nth(1), { shift: true });
  await expect(page.getByTestId('basket')).toHaveText('2 items marked for cleanup');
  await expect(s0.first()).toHaveAttribute('aria-pressed', 'true');

  // Protected items are never added (S3).
  await tools.getByRole('button', { name: 'All' }).click();
  await clickSegment(page, page.locator('[data-kind="item"][data-tier="S3"]').first(), { shift: true });
  await expect(page.getByTestId('announce')).toContainText('is protected; scull never cleans it.');
  await expect(page.getByTestId('basket')).toHaveText('2 items marked for cleanup');
  await shot('cleanup-3-marked');

  // Cleanup Review: nothing changes until the user confirms.
  await page.getByRole('navigation', { name: 'Primary' }).getByRole('link', { name: /Cleanup Review/ }).click();
  await expect(page).toHaveURL(/#\/cleanup$/);
  const confirm = page.getByTestId('confirm');
  await expect(confirm).toBeEnabled();
  await expect(confirm).toHaveText(/^(Delete|Quarantine) \d+/);
  const plan = page.getByRole('region', { name: 'Plan items' });
  // S0 is preselected; protected items are listed but can never be selected.
  await expect(plan.getByRole('checkbox', { checked: true }).first()).toBeVisible();
  await expect(plan.getByText('Protected · never offered')).toBeVisible();
  await shot('cleanup-4-review');

  // Narrow the plan by one item, then confirm.
  const before = await confirm.textContent();
  await plan.getByRole('checkbox', { checked: true }).first().uncheck();
  await expect(confirm).not.toHaveText(before ?? '');
  await confirm.click();
  await expect(page.getByRole('region', { name: 'Confirm' }).getByRole('heading')).toHaveText(/^Cleaning/);
  await shot('cleanup-5-running', { settle: false, show: confirm });

  const done = page.getByTestId('done');
  await expect(done).toBeVisible();
  await expect(done).toContainText('freed');
  await expect(done).toContainText('Measured');
  await expect(page.getByRole('region', { name: 'Confirm' }).getByRole('heading')).toHaveText('Cleanup complete');
  await shot('cleanup-6-done', { show: done });

  await done.getByRole('link', { name: 'Back to overview' }).click();
  await expect(page).toHaveURL(/#\/$/);
});

test('Escape clears the selection and the filter', async ({ page }) => {
  await overviewReady(page);
  const tools = page.getByRole('toolbar', { name: 'Wheel tools' });
  await tools.getByRole('button', { name: 'Costly' }).click();
  await clickSegment(page, page.locator('[data-kind="item"][data-tier="S1"]').first());
  await expect(page.getByTestId('selected').getByTestId('badge')).toHaveText('Costly');
  await page.keyboard.press('Escape');
  await expect(tools.getByRole('button', { name: 'All' })).toHaveAttribute('aria-pressed', 'true');
  await expect(page.getByTestId('selected').getByTestId('badge')).toHaveText('—');
});
