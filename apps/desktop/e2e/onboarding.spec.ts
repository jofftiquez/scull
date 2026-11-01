// First run → onboarding → first scan → overview (PRD §14.7, design 02 / 02b).
import { expect, open, test } from './fixtures';

test('first run opens onboarding, scans the chosen folders, then lands on the overview', async ({ page, shot }) => {
  // A never-scanned machine (mock `?scenario=first-run`): the app must not auto-scan default
  // roots (CFG-04); it opens onboarding instead.
  await open(page, '/?scenario=first-run');
  await expect(page).toHaveURL(/#\/onboarding$/);
  await expect(page).toHaveTitle('Welcome · scull');
  await expect(page.getByRole('heading', { level: 1 })).toHaveText("Let's set scull up for how you work.");
  await expect(page.getByRole('navigation', { name: 'Primary' })).toHaveCount(0);

  // Detection pre-fills the answers; the suggested folders are chosen.
  await expect(page.getByTestId('answered')).toHaveText(/[1-5] of 5 answered/);
  const start = page.getByTestId('start-scan');
  await expect(start).toBeEnabled();
  await expect(page.getByRole('button', { name: 'Web / JS', exact: true })).toHaveAttribute('aria-pressed', 'true');
  await shot('onboarding-1-detected');

  // Change answers, then start the first scan.
  await page.getByRole('button', { name: 'Mobile', exact: true }).click();
  await expect(page.getByRole('button', { name: 'Mobile', exact: true })).toHaveAttribute('aria-pressed', 'true');
  await start.click();
  await expect(page.getByTestId('cancel-scan')).toBeVisible();
  await shot('onboarding-2-scanning', { settle: false, show: page.getByTestId('cancel-scan') });

  const openOverview = page.getByTestId('open-overview');
  await expect(openOverview).toBeVisible();
  await expect(page.getByTestId('save-note')).toHaveCount(0);
  await shot('onboarding-3-first-scan-done', { show: openOverview });

  await openOverview.click();
  await expect(page).toHaveURL(/#\/$/);
  await expect(page).toHaveTitle('Overview · scull');
  await expect(page.getByRole('navigation', { name: 'Primary' })).toBeVisible();
  // The overview reads the scan: the wheel has segments and the rescan button is idle again.
  await expect(page.getByTestId('rescan')).toHaveText(/Rescan/);
  await expect(page.getByTestId('rescan')).toBeEnabled();
  await expect(page.locator('[data-kind="item"]').first()).toBeVisible();
  await expect(page.getByTestId('header')).not.toContainText('not scanned');
  await shot('onboarding-4-overview');
});

test('a machine that was scanned before opens straight on the overview', async ({ page }) => {
  await open(page, '/');
  await expect(page.getByTestId('rescan')).toBeVisible();
  await expect(page).toHaveURL(/#\/$/);
});
