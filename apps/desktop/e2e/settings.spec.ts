// Settings (design 10): a change is saved at once and confirmed with the Saved pill; the
// destructive MCP switch is file-only and locked in the app.
import { expect, open, test } from './fixtures';

test('change a setting, see Saved, and find destructive MCP locked', async ({ page, shot }) => {
  await open(page, '/settings');
  const tabs = page.getByRole('tablist', { name: 'Settings sections' });
  await expect(tabs.getByRole('tab', { name: 'General' })).toHaveAttribute('aria-selected', 'true');

  const widget = page.getByRole('switch', { name: 'Menu bar widget' });
  const was = await widget.getAttribute('aria-checked');
  await widget.click();
  await expect(widget).toHaveAttribute('aria-checked', was === 'true' ? 'false' : 'true');
  await expect(page.getByTestId('saved')).toHaveText('Saved ✓');
  await expect(page.getByTestId('save-error')).toHaveCount(0);
  await shot('settings-1-saved');
  // The pill is a flash: it clears itself.
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
