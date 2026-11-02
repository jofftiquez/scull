// Agent request approval (PRD §13.3, design 12): an MCP client's plan waits for the human, who
// reviews every item and types the confirmation code. Covers the typed code, expiry, denial and
// a target changing (D-34: an MCP plan is voided entirely, never narrowed).
import type { Page } from '@playwright/test';
import { expect, open, test } from './fixtures';

/** The mock's pending request (src/bridge/planMock.ts: MOCK_AGENT_PLAN_ID and its code). */
const PLAN = '9c41e07b';
const CODE = 'K7Q-4MP';

async function openRequest(page: Page) {
  await open(page, `/cleanup?request=${PLAN}`);
  await expect(page.getByTestId('code')).toHaveText(CODE);
}

/** Tick "Reviewed" on every item of the plan. */
async function reviewAll(page: Page) {
  const boxes = page.getByRole('region', { name: 'Plan items' }).getByRole('checkbox', { name: /^Reviewed / });
  const n = await boxes.count();
  expect(n).toBeGreaterThan(0);
  for (let i = 0; i < n; i++) await boxes.nth(i).check();
  await expect(page.getByText(`All ${n} items reviewed`)).toBeVisible();
}

const approve = (page: Page) => page.getByTestId('approve');
const closed = (page: Page, title: string) => page.getByRole('status', { name: title });

test('typed code: a wrong code never approves, the right one approves one run', async ({ page, shot }) => {
  await openRequest(page);
  await expect(page.getByRole('region', { name: 'Who is asking' })).toContainText('Claude Code');
  await expect(page.getByTestId('fp-tag')).toHaveText('Matches');
  await expect(approve(page)).toHaveAttribute('aria-disabled', 'true');
  await shot('agent-1-request');

  await reviewAll(page);
  const input = page.getByTestId('code-input');
  await input.fill('K7Q-4MX');
  await expect(page.getByText("That code doesn't match. Check each character.")).toBeVisible();
  await expect(approve(page)).toHaveAttribute('aria-disabled', 'true');
  // Pressing Approve while not ready changes nothing (the button is aria-disabled, not
  // disabled, so it stays focusable and can say why; Playwright needs `force` to press it).
  await approve(page).click({ force: true });
  await expect(page.getByRole('region', { name: 'Approve or deny' })).toBeVisible();
  await expect(page.getByRole('region', { name: 'Approved' })).toHaveCount(0);
  await shot('agent-2-code-mismatch', { show: input });

  // Case and the dash don't matter.
  await input.fill('k7q4mp');
  await expect(page.getByText('Code matches')).toBeVisible();
  await expect(approve(page)).toHaveAttribute('aria-disabled', 'false');
  await shot('agent-3-ready', { show: input });

  await approve(page).click();
  const approved = page.getByRole('region', { name: 'Approved' });
  await expect(approved.getByRole('heading')).toHaveText('Approved for one run');
  await expect(page.getByTestId('revoke')).toBeVisible();
  await shot('agent-4-approved', { show: approved });

  // The agent runs it; every item is re-checked right before it is touched.
  await expect(approved.getByRole('heading')).toHaveText('Running with your approval');
  const done = page.getByRole('region', { name: 'Cleanup complete' });
  await expect(done).toBeVisible();
  await expect(done).toContainText('freed');
  await expect(done).toContainText('The approval is spent');
  await shot('agent-5-executed', { show: done });
});

test('expired: nobody approved in time, so the plan is void', async ({ page, shot }) => {
  await page.clock.install();
  await openRequest(page);
  await reviewAll(page);
  // The request was made 78 s ago and lives 15 minutes.
  await page.clock.fastForward('15:00');
  const status = closed(page, 'This request expired · nothing changed');
  await expect(status).toBeVisible();
  await expect(status).toContainText('expired · 0 items touched');
  await expect(page.getByTestId('timer-big')).toHaveText('0:00');
  await expect(approve(page)).toHaveCount(0);
  await expect(page.getByTestId('code-input')).toHaveCount(0);
  await shot('agent-6-expired', { show: status });
});

test('denied: the agent is told, nothing runs', async ({ page, shot }) => {
  await openRequest(page);
  await page.getByTestId('deny').click();
  const status = closed(page, 'You denied this request · nothing changed');
  await expect(status).toBeVisible();
  await expect(status).toContainText('denied via desktop · 0 items touched');
  await expect(approve(page)).toHaveCount(0);
  await expect(page.getByRole('region', { name: 'Plan items' }).getByRole('checkbox', { name: /^Reviewed / }).first()).toBeDisabled();
  await shot('agent-7-denied', { show: status });
});

test('plan changed (D-34): a target changing voids the whole MCP plan', async ({ page, shot }) => {
  await openRequest(page);
  await reviewAll(page);
  await page.getByTestId('code-input').fill(CODE);
  await expect(approve(page)).toHaveAttribute('aria-disabled', 'false');

  // The mock's test hook (dev and E2E builds only): a target is modified on disk.
  const hooked = await page.evaluate((id) => {
    const plans = (globalThis as { __scullMockPlans?: { simulateTargetChanged(id: string): void } }).__scullMockPlans;
    if (!plans) return false;
    plans.simulateTargetChanged(id);
    return true;
  }, PLAN);
  expect(hooked, 'the E2E build exposes the mock plan bridge (VITE_SCULL_E2E=1)').toBe(true);

  const status = closed(page, 'This plan changed since it was prepared');
  await expect(status).toBeVisible();
  await expect(status).toContainText("can't be approved");
  await expect(status).toContainText('invalidated (target changed) · 0 items touched');
  // Voided entirely: no approval, no partial run of the unchanged items.
  await expect(approve(page)).toHaveCount(0);
  await expect(page.getByRole('region', { name: 'Approved' })).toHaveCount(0);
  const plan = page.getByRole('region', { name: 'Plan items' });
  await expect(plan.getByText('Changed', { exact: true })).toHaveCount(1);
  await expect(plan).toContainText('plan no longer matches the disk');
  await shot('agent-8-plan-changed');
});

test('notification: scull shows the request anywhere; Review opens it, never approves', async ({ page, shot }) => {
  // `?agent=request`: the mock announces its pending request (inside Tauri the core's broker
  // lists the plans an AI agent prepared through MCP).
  await open(page, '/?agent=request');
  const notice = page.getByRole('region', { name: 'Agent request' });
  await expect(notice).toBeVisible();
  await expect(notice.getByRole('heading')).toHaveText(/^Claude Code wants to clean \d/);
  await expect(notice).toContainText('5 items · plan 9c41·e07b · decide within');
  await expect(notice).toContainText('Nothing runs until you approve in scull.');
  // No Approve or Deny here: approving needs the full plan, every item reviewed and the code.
  await expect(notice.getByRole('button')).toHaveText(['Review', 'Later']);
  await shot('agent-0-notification', { show: notice });

  await notice.getByRole('button', { name: 'Review' }).click();
  await expect(page).toHaveURL(new RegExp(`/cleanup\\?request=${PLAN}`));
  await expect(page.getByTestId('code')).toHaveText(CODE);
  await expect(approve(page)).toHaveAttribute('aria-disabled', 'true');
  await expect(notice).toHaveCount(0);

  // Still undecided, so a fresh window shows it again (Review hid it in this one); Later hides
  // it for this request.
  await open(page, '/projects?agent=request');
  await page.reload();
  await expect(notice).toBeVisible();
  await notice.getByRole('button', { name: 'Later' }).click();
  await expect(notice).toHaveCount(0);
});
