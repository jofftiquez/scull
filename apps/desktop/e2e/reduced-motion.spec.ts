// prefers-reduced-motion: reduce. Every animation collapses to 1 ms (tokens.motion.reducedMotion),
// the onboarding scan doesn't hold its bar for the design's ease, and the end states of the
// key flows appear without anything still moving.
import { expect, open, runningAnimations, test } from './fixtures';

test.use({ reducedMotion: 'reduce' });

test('key flows reach their end states with no animation running', async ({ page, shot }) => {
  await open(page, '/?scenario=first-run');
  await expect(page).toHaveURL(/#\/onboarding$/);
  for (let step = 1; step < 6; step++) await page.getByTestId('survey-next').click();
  await page.getByTestId('start-scan').click();
  const openOverview = page.getByTestId('open-overview');
  await expect(openOverview).toBeVisible();
  // Survey transitions and scan progress honor reduced motion.
  expect(await runningAnimations(page)).toEqual([]);
  await shot('reduced-motion-1-onboarding-done', { settle: false, show: openOverview });

  await openOverview.click();
  await expect(page.getByTestId('rescan')).toHaveText(/Scan/);
  await expect(page.getByTestId('rescan')).toBeEnabled();
  await expect(page.locator('[data-kind="item"]').first()).toBeVisible();
  expect(await runningAnimations(page)).toEqual([]);
  await shot('reduced-motion-2-overview', { settle: false });

  await page.getByRole('navigation', { name: 'Primary' }).getByRole('link', { name: /Cleanup Review/ }).click();
  await expect(page.getByTestId('confirm')).toBeEnabled();
  await page.getByTestId('confirm').click();
  await page.getByTestId('confirm-action').click();
  await expect(page.getByTestId('done')).toBeVisible();
  expect(await runningAnimations(page)).toEqual([]);
  await shot('reduced-motion-3-cleanup-done', { settle: false, show: page.getByTestId('done') });
});

test('Quick Clean finishes without its burst and bounce animating', async ({ page, shot }) => {
  await open(page, '/caches?demo=design');
  await page.getByTestId('start').click();
  await page.getByTestId('confirm-action').click();
  const done = page.getByTestId('task-success');
  await expect(done).toBeVisible();
  expect(await runningAnimations(page)).toEqual([]);
  await shot('reduced-motion-4-quick-clean-done', { settle: false });
});
