// History restore (design 09): the original path is taken again, so the quarantined item comes
// back beside it as `<name>.scull-restored-1` (never overwrites). The mock moves nothing.
import { expect, open, test } from './fixtures';

test('restore a quarantined item next to the occupied original (sibling restore)', async ({ page, shot }) => {
  await open(page, '/history');
  await page.locator('button[data-row]').filter({ hasText: 'Cleanup · 4 items' }).click();
  const details = page.getByTestId('details');
  const restore = details.getByRole('button', { name: 'Restore ~/code/proto/Library' });
  await expect(restore).toBeVisible();
  const restorable = page.getByTestId('restorable');
  const before = Number(await restorable.textContent());
  expect(before).toBeGreaterThan(0);
  await expect(details.getByTestId('chain')).toContainText('hash chain verified');
  await shot('history-1-operation');

  await restore.click();
  await expect(details.getByText('restoring…', { exact: true })).toBeVisible();
  const toast = page.getByTestId('task-success');
  await expect(toast).toContainText('Restored to ~/code/proto/Library.scull-restored-1');
  await expect(restorable).toHaveText(String(before - 1));
  await expect(restore).toHaveCount(0);
  await expect(details.locator('li', { hasText: '~/code/proto/Library' }).getByTestId('item-state')).toContainText(/restored/i);
  await shot('history-2-sibling-restored');
});
