// Caches Quick Clean (design 08), including the orphaned-cache confirmation (D-36): a cache
// whose tool isn't installed needs a per-tool "Include it" before it can be ticked.
import { expect, open, test } from './fixtures';

test('Quick Clean with an orphaned cache confirmed and another kept', async ({ page, shot }) => {
  await open(page, '/caches?demo=design');
  const hero = page.getByTestId('hero');
  await expect(hero.getByTestId('start')).toBeEnabled();
  await expect(hero).toContainText('4 caches');
  await expect(page.getByTestId('skipped-row')).toHaveCount(3);
  await shot('caches-1-preview');

  // Orphaned (tool not installed): ticking asks first, and focus moves into the question.
  const playwright = page.getByRole('checkbox', { name: 'Include Playwright browsers' });
  await playwright.click();
  const ask = page.getByRole('alertdialog', { name: 'Remove Playwright browsers cache' });
  await expect(ask).toBeVisible();
  await expect(ask).toContainText('Not undoable.');
  await expect(ask.getByRole('button', { name: 'Include it' })).toBeFocused();
  await expect(playwright).not.toBeChecked();
  await shot('caches-2-orphan-confirm');
  await ask.getByRole('button', { name: 'Include it' }).click();
  await expect(ask).toHaveCount(0);
  await expect(playwright).toBeChecked();
  await expect(hero).toContainText('5 caches');

  // "Keep it" leaves the other orphaned cache out.
  const cypress = page.getByRole('checkbox', { name: 'Include Cypress binaries' });
  await cypress.click();
  const ask2 = page.getByRole('alertdialog', { name: 'Remove Cypress binaries cache' });
  await ask2.getByRole('button', { name: 'Keep it' }).click();
  await expect(ask2).toHaveCount(0);
  await expect(cypress).not.toBeChecked();
  await expect(hero).toContainText('5 caches');

  // Skipped caches can never be ticked.
  await expect(page.getByRole('checkbox', { name: 'Hugging Face is skipped' })).toBeDisabled();

  await hero.getByTestId('start').click();
  await expect(page.getByTestId('row-state').first()).toBeVisible();
  await shot('caches-3-running', { settle: false });
  const done = hero.getByTestId('done');
  await expect(done).toContainText('freed from 5 caches');
  await expect(done).toContainText('measured');
  await shot('caches-4-done');

  await done.getByRole('button', { name: 'Nice' }).click();
  await expect(done).toHaveCount(0);
  await expect(playwright).toBeDisabled();
});
