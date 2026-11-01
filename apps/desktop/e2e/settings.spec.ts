// Settings (design 10): a change is saved at once and confirmed with the Saved pill; the
// destructive MCP switch is file-only and locked in the app.
//
// The Saved pill is a flash: the page clears it with a 1.5 s setTimeout. On a busy machine the
// click, the aria-checked check and the first poll of the pill can take longer than that, and
// the assertion then waits for a pill that is already gone (#290). So the test owns the page's
// clock: it stands still while the pill is checked and screenshotted, and moves on exactly
// 1.5 s to prove the pill clears itself. No sleeps, no retries.
import { expect, open, test } from './fixtures';

const SAVED_FLASH_MS = 1500;

test('change a setting, see Saved, and find destructive MCP locked', async ({ page, shot }) => {
  await page.clock.install();
  await open(page, '/settings');
  const tabs = page.getByRole('tablist', { name: 'Settings sections' });
  await expect(tabs.getByRole('tab', { name: 'General' })).toHaveAttribute('aria-selected', 'true');
  // Loaded (the mock bridge's latency timers have run): freeze the page's timers from here on.
  await page.clock.pauseAt(await page.evaluate(() => Date.now() + 1000));

  const widget = page.getByRole('switch', { name: 'Menu bar widget' });
  const was = await widget.getAttribute('aria-checked');
  await widget.click();
  await expect(widget).toHaveAttribute('aria-checked', was === 'true' ? 'false' : 'true');
  await expect(page.getByTestId('saved')).toHaveText('Saved ✓');
  await expect(page.getByTestId('save-error')).toHaveCount(0);
  await shot('settings-1-saved');
  // The pill is a flash: still there just before 1.5 s, cleared by itself at 1.5 s.
  await page.clock.runFor(SAVED_FLASH_MS - 1);
  await expect(page.getByTestId('saved')).toHaveText('Saved ✓');
  await page.clock.runFor(1);
  await expect(page.getByTestId('saved')).toHaveText('');

  await tabs.getByRole('tab', { name: 'AI agents' }).click();
  await expect(page).toHaveURL(/tab=agents/);
  const destructive = page.getByRole('switch', { name: 'Let agents prepare and run cleanup plans' });
  await expect(destructive).toBeDisabled();
  await expect(destructive).toHaveAttribute('aria-checked', 'false');
  await expect(page.getByText('Only config.toml plus the --allow-destructive launch flag can turn this on.')).toBeVisible();
  // A forced click on the locked switch changes nothing and saves nothing.
  await destructive.click({ force: true });
  await expect(destructive).toHaveAttribute('aria-checked', 'false');
  await expect(page.getByTestId('saved')).toHaveText('');

  // The read-only MCP switch next to it saves like any other.
  const mcp = page.getByRole('switch', { name: 'Let agents use scull' });
  const mcpWas = await mcp.getAttribute('aria-checked');
  await mcp.click();
  await expect(mcp).toHaveAttribute('aria-checked', mcpWas === 'true' ? 'false' : 'true');
  await expect(page.getByTestId('saved')).toHaveText('Saved ✓');
  await shot('settings-2-locked-destructive-mcp');
});
