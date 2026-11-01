// Tray monitoring and the weekly digest (P5-PRO-01): the Insights "This week" card shows the
// digest of the recorded history; Settings turns the opt-in weekly notification on, pauses and
// resumes alerts and sends the digest now. In the browser build the monitor is the in-memory
// mock: nothing is written to disk and no notification is shown (the real ones come from the
// tray app). The choices are kept in the page's local storage, so a reload reads them back.
import { expect, open, test } from './fixtures';

test('weekly digest: read it on Insights, turn it on, pause alerts, send it now', async ({ page, shot }) => {
  await open(page, '/insights');
  const card = page.getByTestId('digest');
  await expect(card).toBeVisible();
  await expect(card).toContainText(/41\.2 GiB free on \/ \(−24\.5 GiB this week\), about \d+ days until 10\.0 GiB free/);
  await expect(card).toContainText('Grew most: ML model caches');
  await expect(page.getByTestId('digest-note')).toContainText('The weekly digest is off');
  await shot('digest-1-insights-off', { show: card });

  await card.getByRole('link', { name: 'Turn on the weekly digest' }).click();
  await expect(page).toHaveURL(/\/settings\?tab=alerts/);
  const digest = page.getByRole('switch', { name: 'Weekly digest' });
  await expect(digest).toHaveAttribute('aria-checked', 'false');
  await expect(page.getByTestId('next-digest')).toHaveText('Turn on Weekly digest to get it every Monday.');
  await digest.click();
  await expect(digest).toHaveAttribute('aria-checked', 'true');
  await expect(page.getByTestId('save-error')).toHaveCount(0);
  await expect(page.getByTestId('next-digest')).toHaveText(/^Next digest (today|tomorrow|Mon) \d\d:\d\d\.$/);

  const monitoring = page.getByTestId('monitoring');
  await monitoring.getByRole('group', { name: 'Look every' }).getByRole('button', { name: '15 min' }).click();
  await expect(monitoring.getByRole('button', { name: '15 min' })).toHaveAttribute('aria-pressed', 'true');
  await monitoring.getByRole('button', { name: '1 hour' }).click();
  await expect(page.getByTestId('pause-state')).toHaveText(/^Paused until (today|tomorrow) \d\d:\d\d\.$/);
  await expect(monitoring.getByRole('button', { name: 'Resume' })).toBeVisible();
  await shot('digest-2-settings-on-paused', { show: monitoring });

  await monitoring.getByRole('button', { name: 'Resume' }).click();
  await expect(page.getByTestId('pause-state')).toHaveText('Alerts are on.');
  await monitoring.getByRole('button', { name: 'Send now' }).click();
  await expect(page.getByText("This week's digest is ready (the browser preview shows no notification).")).toBeVisible();

  // Kept on this computer: a reload reads the choices back, and Insights says it is on.
  await page.reload();
  await expect(page.getByRole('switch', { name: 'Weekly digest' })).toHaveAttribute('aria-checked', 'true');
  await expect(page.getByTestId('monitoring').getByRole('button', { name: '15 min' })).toHaveAttribute('aria-pressed', 'true');
  await open(page, '/insights');
  await expect(page.getByTestId('digest-note')).toHaveText(/^Sent as a notification every Monday morning\. Next: /);
  await shot('digest-3-insights-on', { show: page.getByTestId('digest') });
});
