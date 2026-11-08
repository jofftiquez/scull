import { expect, open, test } from './fixtures';

test('confirmed tasks survive navigation, serialize, and keep pending worktrees visible', async ({ page, shot }) => {
  await open(page, '/cleanup');
  await expect(page.getByTestId('confirm')).toBeEnabled();
  await page.evaluate(() => {
    const host = window as unknown as { __scullMockPlans: { executePlan: (...args: unknown[]) => Promise<unknown> }; releaseCleanup: () => void };
    const original = host.__scullMockPlans.executePlan.bind(host.__scullMockPlans);
    let release!: () => void;
    const gate = new Promise<void>((resolve) => { release = resolve; });
    host.releaseCleanup = release;
    host.__scullMockPlans.executePlan = async (...args) => { await gate; return original(...args); };
  });
  await page.getByTestId('confirm').click();
  await page.getByTestId('confirm-action').click();
  const tasks = page.getByTestId('toolbar-tasks');
  await expect(tasks).toContainText('Running');
  await page.getByRole('navigation', { name: 'Primary' }).getByRole('link', { name: /Git Worktrees/ }).click();
  const rows = page.locator('button[data-row]');
  await expect(rows).toHaveCount(8);
  const row = rows.filter({ hasText: 'agent/fix-flaky-tests-7' });
  await row.click();
  await page.getByTestId('remove').click();
  await page.getByRole('textbox', { name: 'Confirm irreversible item count' }).fill('1');
  await page.getByTestId('confirm-action').click();
  await expect(tasks.locator(':scope > summary')).toHaveText('2 tasks');
  await tasks.locator(':scope > summary').click();
  await expect(tasks).toContainText('Queued');
  await page.waitForTimeout(1000); // Past the old premature row-collapse animation.
  await expect(row).toBeVisible();
  await shot('tasks-1-pending');
  await page.evaluate(() => (window as unknown as { releaseCleanup: () => void }).releaseCleanup());
  await expect(rows).toHaveCount(7);
  await expect(tasks.locator(':scope > summary')).toHaveText('Tasks');
  await expect(tasks.locator('.task-row')).toHaveCount(2);
  await expect(tasks.locator('.task-row').filter({ has: page.locator('.status-chip[data-status="succeeded"]') })).toHaveCount(2);
  await shot('tasks-2-completed');
  await page.getByRole('navigation', { name: 'Primary' }).getByRole('link', { name: /Settings/ }).click();
  await page.getByRole('navigation', { name: 'Primary' }).getByRole('link', { name: /Git Worktrees/ }).click();
  await expect(page.locator('button[data-row]')).toHaveCount(7);
});

test('shared confirmation cancels safely, restores focus, and ready explorer tiles have color', async ({ page, shot }) => {
  await open(page, '/caches?demo=design');
  const start = page.getByTestId('start');
  await start.click();
  await expect(page.getByRole('dialog')).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(page.getByRole('dialog')).toHaveCount(0);
  await expect(start).toBeFocused();
  await expect(page.getByTestId('toolbar-tasks')).toHaveCount(0);
  await page.getByRole('navigation', { name: 'Primary' }).getByRole('link', { name: /Storage Explorer/ }).click();
  const tile = page.locator('[data-kind="drive"] .bg').first();
  await expect(tile).toBeVisible();
  expect(await tile.getAttribute('fill')).toBe('var(--color-drive-tile)');
  await expect(page.getByTestId('main-scroll')).not.toContainText('click a folder to open');
  await shot('tasks-3-explorer-ready');
});

test('confirmation is centered and selected drawer actions use the full footer', async ({ page, shot }) => {
  await open(page, '/worktrees');
  await page.locator('button[data-row]').filter({ hasText: 'agent/fix-flaky-tests-7' }).click();
  const footer = page.getByTestId('main-footer');
  const headerBox = (await page.getByTestId('header').boundingBox())!;
  const footerBox = (await footer.boundingBox())!;
  expect(footerBox.x).toBe(headerBox.x);
  expect(footerBox.width).toBe(headerBox.width);
  await expect(page.getByRole('complementary', { name: 'Worktree details' }).locator('footer')).toHaveCount(0);
  await page.getByTestId('remove').click();
  const dialog = page.getByRole('dialog');
  await expect(dialog).toBeVisible();
  const box = (await dialog.boundingBox())!;
  const viewport = page.viewportSize()!;
  expect(Math.abs(box.x + box.width / 2 - viewport.width / 2)).toBeLessThan(2);
  expect(Math.abs(box.y + box.height / 2 - viewport.height / 2)).toBeLessThan(2);
  await shot('revision-5-centered-confirmation');
  await page.keyboard.press('Escape');
  await expect(page.getByTestId('remove')).toBeFocused();
  await page.getByRole('navigation', { name: 'Primary' }).getByRole('link', { name: /Projects/ }).click();
  await page.getByTestId('project-row').first().click();
  expect((await footer.boundingBox())!.width).toBe(headerBox.width);
  await expect(footer.locator('footer')).toHaveCount(1);
  await expect(footer.getByRole('button')).toHaveCount(1);
  await shot('revision-5-project-full-footer');
});

test('failed tasks use readable titles, colored statuses and exact failure details', async ({ page, shot }) => {
  await open(page, '/cleanup');
  await expect(page.getByTestId('confirm')).toBeEnabled();
  await page.evaluate(() => {
    const host = window as unknown as { __scullMockPlans: { executePlan: () => Promise<unknown> } };
    host.__scullMockPlans.executePlan = async () => { throw new Error('The scan predates recovery of an interrupted operation. Run Scan again, then review the new plan.'); };
  });
  await page.getByTestId('confirm').click();
  await page.getByTestId('confirm-action').click();
  const tasks = page.getByTestId('toolbar-tasks');
  await expect(tasks.locator(':scope > summary')).toContainText('1 need review');
  await tasks.locator(':scope > summary').click();
  await expect(tasks.locator('.task-row > div > strong')).toHaveText('Clean files');
  await expect(tasks.locator('.status-chip[data-status="failed"]').first()).toHaveText('Failed');
  await expect(tasks.getByText('Why it failed')).toBeVisible();
  await expect(tasks.locator('.result-reason')).toContainText('Run Scan again');
  const badge = tasks.locator('.status-chip[data-status="failed"]').first();
  const colors = await badge.evaluate((el) => ({ text: getComputedStyle(el).color, fill: getComputedStyle(el).backgroundColor }));
  expect(colors.fill).not.toBe('rgba(0, 0, 0, 0)');
  expect(colors.text).not.toBe(colors.fill);
  await shot('revision-5-readable-failure');
});

test('Scan and queued task indicators animate while work is active', async ({ page, shot }) => {
  await open(page, '/worktrees');
  await expect(page.getByTestId('rescan')).toBeEnabled();
  const scan = page.getByTestId('rescan');
  expect(await scan.evaluate((el) => getComputedStyle(el).backgroundColor)).not.toBe('rgb(21, 18, 26)');
  await scan.click();
  await expect(scan).toHaveAttribute('aria-busy', 'true');
  expect(await scan.locator('svg').evaluate((el) => el.getAnimations().some((a) => a.playState === 'running'))).toBe(true);
  await shot('revision-5-scan-active', { settle: false });
  await expect(scan).toBeEnabled();
});

test('Escape cancels Overview confirmation without closing the selected drawer', async ({ page }) => {
  await open(page, '/?scenario=design');
  await expect(page.getByTestId('rescan')).toBeEnabled();
  await page.locator('[data-kind="category"]').first().press('Enter');
  const aside = page.getByRole('complementary', { name: 'Details', exact: true });
  await expect(aside).toBeVisible();
  const clean = page.getByTestId('qc-button');
  await clean.click();
  await expect(page.getByRole('dialog')).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(page.getByRole('dialog')).toHaveCount(0);
  await expect(aside).toBeVisible();
  await expect(clean).toBeFocused();
});

test('cache task shows completed-tool counts and live output across page changes', async ({ page, shot }) => {
  await open(page, '/caches?demo=design');
  await expect(page.getByTestId('start')).toBeEnabled();
  await page.clock.install();
  await page.clock.pauseAt(new Date());
  await page.getByTestId('start').click();
  await page.getByTestId('confirm-action').click();
  const tasks = page.getByTestId('toolbar-tasks');
  await expect(tasks.locator(':scope > summary')).toContainText('0 / 4 tools');
  await tasks.locator(':scope > summary').click();
  await expect(tasks).toContainText('0 / 4 tools finished');
  await expect(tasks.getByTestId('task-output')).toContainText('Simulated cleanup output');
  await expect(tasks.getByTestId('task-output')).toContainText('npm cache verify');
  await page.clock.runFor(800);
  await expect(tasks).toContainText('1 / 4 tools finished');
  await shot('cache-live-1-progress', { settle: false });
  await page.getByRole('navigation', { name: 'Primary' }).getByRole('link', { name: /Git Worktrees/ }).click();
  await page.clock.runFor(200);
  await tasks.locator(':scope > summary').click();
  await expect(tasks).toContainText('1 / 4 tools finished');
  await expect(tasks.getByTestId('task-output')).toContainText('Simulated cleanup output');
  await page.clock.runFor(3000);
  await expect(tasks).toContainText('4 / 4 tools finished');
  await expect(tasks.locator('.status-chip[data-status="succeeded"]')).toHaveText('Completed');
  await tasks.getByText('Command output', { exact: true }).click();
  await expect(tasks.getByTestId('task-output')).toContainText('Command completed.');
  await shot('cache-live-2-retained-output', { settle: false });
});
