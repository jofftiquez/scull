import { clickSegment, expect, open, test } from './fixtures';

test.use({ viewport: { width: 1440, height: 640 } });

test('dashboard columns scroll independently while the toolbar and detail actions stay in place', async ({ page, shot }) => {
  await open(page, '/');
  await expect(page.getByTestId('rescan')).toBeEnabled();
  await expect(page.locator('aside')).toHaveCount(0);
  await clickSegment(page, page.locator('[data-kind="category"]').first());
  expect(await page.evaluate(() => document.documentElement.scrollHeight <= innerHeight + 1)).toBe(true);
  const main = page.getByTestId('main-scroll');
  const left = page.getByTestId('sidebar-scroll');
  const right = page.getByTestId('aside-scroll');
  const toolbar = page.getByTestId('header');
  const action = page.getByTestId('main-footer');
  const before = { toolbar: await toolbar.boundingBox(), action: await action.boundingBox() };
  for (const pane of [main, left, right]) {
    expect(await pane.evaluate((el) => el.scrollHeight > el.clientHeight)).toBe(true);
  }
  await main.evaluate((el) => { el.scrollTop = el.scrollHeight; });
  expect(await main.evaluate((el) => el.scrollTop)).toBeGreaterThan(0);
  expect(await left.evaluate((el) => el.scrollTop)).toBe(0);
  expect(await right.evaluate((el) => el.scrollTop)).toBe(0);
  await left.evaluate((el) => { el.scrollTop = el.scrollHeight; });
  const mainTop = await main.evaluate((el) => el.scrollTop);
  await right.evaluate((el) => { el.scrollTop = el.scrollHeight; });
  expect(await main.evaluate((el) => el.scrollTop)).toBe(mainTop);
  expect(await page.evaluate(() => document.scrollingElement?.scrollTop)).toBe(0);
  expect((await toolbar.boundingBox())!.y).toBe(before.toolbar!.y);
  expect((await action.boundingBox())!.y).toBe(before.action!.y);
  await right.focus();
  await right.evaluate((el) => { el.scrollTop = 0; });
  await page.keyboard.press('PageDown');
  await expect.poll(() => right.evaluate((el) => el.scrollTop)).toBeGreaterThan(0);
  expect(await main.evaluate((el) => el.scrollTop)).toBe(mainTop);
  await shot('11-dashboard-independent-panes');
});

test('cleanup confirmation stays at the bottom while review content scrolls', async ({ page, shot }) => {
  await open(page, '/cleanup');
  const confirm = page.getByTestId('confirm');
  await expect(confirm).toBeVisible();
  const before = (await confirm.boundingBox())!;
  expect(before.y + before.height).toBeLessThanOrEqual(640);
  await page.getByTestId('main-scroll').evaluate((el) => { el.scrollTop = el.scrollHeight; });
  expect((await confirm.boundingBox())!.y).toBe(before.y);
  await expect(page.getByTestId('main-footer')).toContainText('predicted');
  await confirm.click();
  await expect(page.getByRole('dialog')).toContainText('cannot be undone');
  await page.getByRole('dialog').getByRole('button', { name: 'Cancel', exact: true }).click();
  await expect(confirm).toBeFocused();
  await shot('12-dashboard-fixed-confirmation');
});

test('the three-pane layout fits the minimum desktop width without document overflow', async ({ page, shot }) => {
  await page.setViewportSize({ width: 960, height: 640 });
  await open(page, '/');
  await expect(page.getByTestId('rescan')).toBeEnabled();
  await expect(page.locator('aside')).toHaveCount(0);
  await clickSegment(page, page.locator('[data-kind="category"]').first());
  const left = (await page.getByTestId('sidebar').boundingBox())!;
  const main = (await page.getByTestId('main-scroll').boundingBox())!;
  const right = (await page.getByRole('complementary', { name: 'Details', exact: true }).boundingBox())!;
  expect(left.x + left.width).toBeLessThanOrEqual(main.x);
  expect(main.x + main.width).toBeLessThanOrEqual(right.x + 1);
  expect(right.x + right.width).toBeLessThanOrEqual(960);
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  await shot('13-dashboard-minimum-width');
  await page.setViewportSize({ width: 1440, height: 900 });
  await shot('14-dashboard-overview');
});


test('global Scan is clean and list details open only after selection', async ({ page, shot }) => {
  await open(page, '/projects?demo=design');
  const header = page.getByTestId('header');
  await expect(header.getByRole('button', { name: 'Scan', exact: true })).toBeVisible();
  await expect(header).not.toContainText(/scanned|reclaimable|folders|Show regenerable/);
  await expect(page.getByTestId('sidebar')).not.toContainText('Supporter');
  await expect(page.locator('aside')).toHaveCount(0);
  await expect(page.getByTestId('project-row').first()).toBeVisible();
  await expect(page.getByTestId('filter')).toHaveCount(5);
  const more = page.getByRole('combobox', { name: 'More project filters' });
  await expect(more).toBeVisible();
  await more.selectOption({ index: 1 });
  await expect(page.locator('[data-testid="filter"][aria-pressed="true"]')).toHaveCount(0);
  await page.getByTestId('filter').first().click();
  const full = (await page.getByTestId('main-scroll').boundingBox())!.width;
  await shot('15-projects-full-width');
  const row = page.getByTestId('project-row').first();
  await row.click();
  await expect(page.locator('aside')).toBeVisible();
  expect((await page.getByTestId('main-scroll').boundingBox())!.width).toBeLessThan(full);
  await page.getByRole('button', { name: 'Close details' }).click();
  await expect(page.locator('aside')).toHaveCount(0);
  await expect(row).toBeFocused();
  expect((await page.getByTestId('main-scroll').boundingBox())!.width).toBe(full);
  await page.getByRole('link', { name: /Git Worktrees/ }).click();
  await expect(header.getByRole('button', { name: 'Scan', exact: true })).toBeEnabled();
  await header.getByRole('button', { name: 'Scan', exact: true }).click();
  await expect(header.getByRole('button', { name: 'Scanning…', exact: true })).toBeDisabled();
  await expect(header.getByRole('button', { name: 'Scan', exact: true })).toBeEnabled();
  await expect(page.locator('button[data-row]')).toHaveCount(8);
  await expect(page.locator('aside')).toHaveCount(0);
  await shot('16-worktrees-refreshed');
});
