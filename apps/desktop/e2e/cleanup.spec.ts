// Overview tier filter + selection → add to cleanup → Cleanup Review → confirm → done (mock).
// Design 01 (wheel) and 03 / 03b (review). The mock "cleans" with timers; nothing is deleted.
import type { Page } from '@playwright/test';
import { clickSegment, expect, open, test } from './fixtures';

/** The Overview with the design's machine, once its first scan (and the radar sweep) is done. */
async function overviewReady(page: Page) {
  await open(page, '/?scenario=design');
  const rescan = page.getByTestId('rescan');
  await expect(rescan).toHaveText(/Scan/);
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
  await expect(page.getByTestId('detail-action')).toHaveText('Add to cleanup');
  await shot('cleanup-2-filtered-and-selected');

  await page.getByTestId('detail-action').click();
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

  const marked = [await s0.first().getAttribute('data-seg'), await s0.nth(1).getAttribute('data-seg')];

  // Cleanup Review plans exactly the marked items (#283); nothing changes until the user confirms.
  await page.getByRole('navigation', { name: 'Primary' }).getByRole('link', { name: /Cleanup Review/ }).click();
  await expect(page).toHaveURL(/#\/cleanup$/);
  const confirm = page.getByTestId('confirm');
  await expect(confirm).toBeEnabled();
  await expect(page.getByTestId('plan-line')).toContainText('from 2 items marked on the Overview');
  const plan = page.getByRole('region', { name: 'Plan items' });
  await expect(plan.locator('[data-item]')).toHaveCount(2);
  expect(await plan.locator('[data-item]').evaluateAll((rows) => rows.map((r) => r.getAttribute('data-item')))).toEqual(marked);
  // Both are Regenerable, so both are preselected; the refused S3 item is not in the plan.
  await expect(plan.getByRole('checkbox', { checked: true })).toHaveCount(2);
  await expect(confirm).toHaveText('Review cleanup');
  await expect(plan.getByText('Protected · never offered')).toHaveCount(0);
  await shot('cleanup-4-review');

  // Narrow the plan by one item, then confirm.
  const before = await page.getByTestId('main-footer').textContent();
  await plan.getByRole('checkbox', { checked: true }).first().uncheck();
  await expect(page.getByTestId('main-footer')).not.toHaveText(before ?? '');
  await confirm.click();
  await page.getByTestId('confirm-action').click();
  await expect(page.getByTestId('toolbar-tasks')).toBeVisible();
  await shot('cleanup-5-running', { settle: false, show: confirm });

  const done = page.getByTestId('done');
  await expect(done).toBeVisible();
  await expect(done).toContainText('freed');
  await expect(done).toContainText('items completed');
  await shot('cleanup-6-done', { show: done });

  await page.getByTestId('main-footer').getByRole('link', { name: 'Overview', exact: true }).click();
  await expect(page).toHaveURL(/#\/$/);
  // The cleaned item left the basket; the unticked one is still marked.
  await expect(page.getByTestId('basket')).toHaveText('1 item marked for cleanup');
});

test('with nothing marked, Cleanup Review plans from the whole last scan', async ({ page, shot }) => {
  await overviewReady(page);
  await expect(page.getByTestId('basket')).toHaveCount(0);
  await page.getByRole('navigation', { name: 'Primary' }).getByRole('link', { name: /Cleanup Review/ }).click();
  await expect(page.getByTestId('confirm')).toBeEnabled();
  await expect(page.getByTestId('plan-line')).toContainText('from the last scan');
  const plan = page.getByRole('region', { name: 'Plan items' });
  // S0 is preselected; protected items are listed but can never be selected.
  await expect(plan.getByRole('checkbox', { checked: true }).first()).toBeVisible();
  await expect(plan.getByText('Protected · never offered')).toBeVisible();
  await shot('cleanup-7-review-from-scan');
});

test('Escape clears the selection and the filter', async ({ page }) => {
  await overviewReady(page);
  const tools = page.getByRole('toolbar', { name: 'Wheel tools' });
  await tools.getByRole('button', { name: 'Costly' }).click();
  await clickSegment(page, page.locator('[data-kind="item"][data-tier="S1"]').first());
  await expect(page.getByTestId('selected').getByTestId('badge')).toHaveText('Costly');
  await page.keyboard.press('Escape');
  await expect(tools.getByRole('button', { name: 'All' })).toHaveAttribute('aria-pressed', 'true');
  await expect(page.locator('aside')).toHaveCount(0);
});
