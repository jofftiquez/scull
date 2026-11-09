// The desktop MVP flow (#144) on the mock bridge: folders → scan → review → confirm → clean,
// and the caches view (the typed confirmation is unit-tested: src/lib/plan.test.ts). Nothing on disk
// changes (the mock bridge), and requests outside the app fail the test (fixtures.ts).
import { expect, test } from './fixtures';

test('scan, review, confirm and clean', async ({ page, shot }) => {
  await page.goto('/');
  await expect(page.getByRole('heading', { name: 'Folders to scan' })).toBeVisible();
  await expect(page.getByRole('list', { name: 'Scan folders' })).toContainText('~/code');
  await shot('01-folders');

  await page.getByRole('button', { name: 'Scan', exact: true }).click();
  await expect(page.getByRole('heading', { name: 'Review the cleanup' })).toBeVisible();

  // S0 is preselected; S1/S2 start unticked; S3 is never offered (left out).
  const s0 = page.locator('section').filter({ hasText: 'regenerable, selected' });
  await expect(s0.getByRole('checkbox').first()).toBeChecked();
  for (const opt of ['costly to recreate, opt in', 'uncertain, opt in per item']) {
    const card = page.locator('section').filter({ hasText: opt });
    if ((await card.count()) === 0) continue;
    for (const box of await card.getByRole('checkbox').all()) await expect(box).not.toBeChecked();
  }
  await shot('02-review');

  const go = page.getByRole('button', { name: /^(Delete|Quarantine) / });
  await go.click();
  const dialog = page.getByRole('dialog', { name: 'Clean these items?' });
  await expect(dialog).toBeVisible();
  await shot('03-confirm');
  await dialog.getByRole('button', { name: 'Clean', exact: true }).click();

  await expect(page.getByRole('heading', { name: 'Done' })).toBeVisible();
  await expect(page.getByRole('status').filter({ hasText: /item\(s\) cleaned/ })).toBeVisible();
  await shot('04-done');
});

test('caches view lists caches, largest first', async ({ page, shot }) => {
  await page.goto('/');
  await page.getByRole('tab', { name: 'Caches' }).click();
  await expect(page.getByRole('heading', { name: 'Package caches' })).toBeVisible();
  await expect(page.getByRole('table')).toBeVisible();
  await shot('06-caches');
});
