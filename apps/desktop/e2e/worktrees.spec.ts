// Git Worktrees (design 04): remove a Removable worktree (branch kept) through a one-item plan;
// protected or review worktrees can't be removed. The mock only animates.
import { expect, open, test } from './fixtures';

test('remove a removable worktree and keep its branch', async ({ page, shot }) => {
  await open(page, '/worktrees');
  // The summary only shows counts once the view has loaded (it reads "Reading worktrees…" before),
  // so the rows are rendered by the time it matches; count them only after that.
  const summary = page.getByTestId('worktrees-summary');
  await expect(summary).toContainText(/\d+ worktrees? across/);
  const list = page.getByRole('region', { name: 'Worktree list' });
  const details = page.getByRole('region', { name: 'Worktree details' });
  const rows = list.locator('button[data-row]');
  const before = Number((await summary.textContent())!.trim().match(/^(\d+) worktree/)![1]);
  expect(before).toBeGreaterThan(1);
  await expect(rows).toHaveCount(before);

  // A protected one first: whole-worktree removal is blocked.
  await rows.filter({ has: page.locator('[data-verdict="protected"]') }).first().click();
  await expect(details.getByTestId('remove')).toBeDisabled();
  await expect(details.getByTestId('remove')).toHaveText('Whole-worktree removal blocked');

  const removable = rows.filter({ has: page.locator('[data-verdict="removable"]') }).first();
  await removable.click();
  await expect(removable).toHaveAttribute('aria-current', 'true');
  const branch = (await details.getByRole('heading', { level: 2 }).textContent())!.trim();
  const remove = details.getByTestId('remove');
  await expect(remove).toBeEnabled();
  await expect(remove).toHaveText(/^Remove worktree, keep branch · /);
  await expect(details.getByTestId('verdict')).toBeVisible();
  await shot('worktrees-1-removable');

  await remove.click();
  await expect(remove).toHaveText('Removing worktree…');
  const toast = page.getByTestId('toast');
  await expect(toast).toContainText(`Removed ${branch} · branch kept`);
  await expect(rows).toHaveCount(before - 1);
  await expect(list.getByText(branch, { exact: true })).toHaveCount(0);
  await expect(summary).toContainText(`${before - 1} worktrees`);
  await shot('worktrees-2-removed');
});
