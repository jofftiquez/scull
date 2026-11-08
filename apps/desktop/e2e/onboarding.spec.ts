import { expect, open, tabTo, test } from './fixtures';
import type { Page } from '@playwright/test';

async function continueToReview(page: Page) {
  for (let step = 1; step < 6; step++) await page.getByTestId('survey-next').click();
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('Ready for your first scan?');
}

test('six steps preserve choices, pick folders, review, scan, and open the overview', async ({ page, shot }) => {
  await open(page, '/?scenario=first-run');
  await expect(page).toHaveURL(/#\/onboarding$/);
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('What do you build?');
  await expect(page.getByRole('navigation', { name: 'Primary' })).toHaveCount(0);
  await expect(page.getByTestId('start-scan')).toHaveCount(0);
  await page.getByRole('checkbox', { name: 'Mobile', exact: true }).check();
  await shot('01-onboarding-work');
  await page.getByTestId('survey-next').click();
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('Where do your projects live?');
  await page.getByTestId('choose-folders').click();
  await expect(page.getByTestId('folder').filter({ hasText: '~/Projects' })).toBeVisible();
  await expect(page.locator('input[type="text"]')).toHaveCount(0);
  await shot('02-onboarding-folders');
  await page.getByTestId('survey-back').click();
  await expect(page.getByRole('checkbox', { name: 'Mobile', exact: true })).toBeChecked();
  await page.getByTestId('survey-next').click();
  await page.getByTestId('survey-next').click();
  await page.getByRole('radio', { name: 'Sometimes', exact: true }).check();
  await shot('03-onboarding-agents');
  await page.getByTestId('survey-next').click();
  await page.getByRole('radio', { name: /Just understand my disk/ }).check();
  await shot('04-onboarding-goal');
  await page.getByTestId('survey-next').click();
  await page.getByRole('radio', { name: /Very cautious/ }).check();
  await shot('05-onboarding-preference');
  await page.getByTestId('survey-next').click();
  await expect(page.getByTestId('review-row')).toHaveCount(5);
  await expect(page.getByTestId('summary')).toContainText('Sometimes');
  await page.getByTestId('edit-step-2').click();
  await expect(page.getByTestId('folder').filter({ hasText: '~/Projects' }).getByRole('checkbox')).toBeChecked();
  for (let step = 2; step < 6; step++) await page.getByTestId('survey-next').click();
  await shot('06-onboarding-review');
  await page.getByTestId('start-scan').click();
  await expect(page.getByTestId('cancel-scan')).toBeVisible();
  await expect(page.getByRole('progressbar')).not.toHaveAttribute('aria-valuenow');
  await shot('07-onboarding-scanning', { settle: false });
  await expect(page.getByTestId('open-overview')).toBeVisible();
  await expect(page.getByTestId('scan')).toContainText('Scan complete');
  await shot('08-onboarding-complete');
  await page.getByTestId('open-overview').click();
  await expect(page).toHaveURL(/#\/$/);
  await expect(page.getByTestId('rescan')).toBeEnabled();
  await expect(page.getByRole('navigation', { name: 'Primary' })).toBeVisible();
  await shot('09-onboarding-overview');

  // The browser mock has no persistent database, but app-local choices survive reload.
  await open(page, '/onboarding');
  await expect(page.getByRole('checkbox', { name: 'Mobile', exact: true })).toBeChecked();
  await page.getByTestId('survey-next').click();
  await expect(page.getByTestId('folder').filter({ hasText: '~/Projects' }).getByRole('checkbox')).toBeChecked();
});

test('empty answers block Continue and setup can be deferred without scanning', async ({ page }) => {
  await open(page, '/?scenario=first-run');
  await expect(page.getByRole('checkbox', { name: 'Web / JS', exact: true })).toBeChecked();
  for (const box of await page.getByRole('checkbox').all()) await box.uncheck();
  await expect(page.getByTestId('survey-next')).toBeDisabled();
  await page.getByRole('checkbox', { name: 'Something else' }).check();
  await page.getByTestId('survey-next').click();
  for (const box of await page.getByRole('checkbox').all()) await box.uncheck();
  await expect(page.getByTestId('survey-next')).toBeDisabled();
  await page.getByTestId('skip').click();
  await expect(page).toHaveURL(/#\/$/);
  expect(await page.evaluate(() => localStorage.getItem('scull.onboarding.setup'))).toBeNull();
  await expect(page.getByTestId('header')).toContainText('Scan');
});

test('keyboard navigation moves focus to each question at the minimum window size', async ({ page, shot }) => {
  await page.setViewportSize({ width: 960, height: 640 });
  await page.emulateMedia({ reducedMotion: 'reduce' });
  await open(page, '/?scenario=first-run');
  for (let step = 1; step < 6; step++) {
    await tabTo(page, page.getByTestId('survey-next'));
    await page.keyboard.press('Enter');
    await expect(page.getByRole('heading', { level: 1 })).toBeFocused();
    await expect(page.getByTestId('survey-progress')).toContainText(`${step + 1} of 6`);
  }
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  await shot('10-onboarding-keyboard-minimum');
});

test('stopping a scan offers partial results and retry', async ({ page }) => {
  await open(page, '/?scenario=first-run');
  await continueToReview(page);
  await page.getByTestId('start-scan').click();
  await page.getByTestId('cancel-scan').click();
  await expect(page.getByTestId('scan')).toContainText('partial results');
  await expect(page.getByTestId('retry-scan')).toBeVisible();
  await expect(page.getByTestId('open-overview')).toContainText('View partial results');
});

test('a machine that was scanned before opens straight on the overview', async ({ page }) => {
  await open(page, '/');
  await expect(page.getByTestId('rescan')).toBeVisible();
  await expect(page).toHaveURL(/#\/$/);
});
