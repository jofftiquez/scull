// Shared fixtures and helpers for the E2E flows. Every wait is on a locator or a page
// condition; no test sleeps for a fixed time.
import { mkdir } from 'node:fs/promises';
import { join } from 'node:path';
import { test as base, expect, type Locator, type Page } from '@playwright/test';

/** Where `shot()` writes its PNGs (run.sh copies them into `.evidence/<issue>/`). */
const SHOTS = process.env.E2E_SHOTS_DIR ?? join(import.meta.dirname, 'shots');

export interface Fixtures {
  /** Scroll `show` into view (if given), settle (unless `settle: false`, for a mid-animation
   *  state), then screenshot the page into SHOTS and attach it to the test report. */
  shot: (name: string, opts?: { settle?: boolean; show?: Locator }) => Promise<void>;
  /** Fails the test on page errors, console errors and any request leaving the app's origin. */
  guard: void;
}

export const test = base.extend<Fixtures>({
  guard: [
    async ({ page, baseURL }, use) => {
      const origin = new URL(baseURL ?? 'http://localhost').origin;
      const problems: string[] = [];
      // Safety (PRD §10): the app under test is the browser build with the mock bridge. Nothing
      // may reach the network; requests to other origins are aborted and fail the test.
      await page.route('**/*', (route) => {
        const url = route.request().url();
        if (url.startsWith(origin) || url.startsWith('data:') || url.startsWith('blob:')) return route.continue();
        problems.push(`request outside the app: ${url}`);
        return route.abort();
      });
      page.on('pageerror', (e) => problems.push(`page error: ${e.message}`));
      page.on('console', (m) => {
        if (m.type() === 'error') problems.push(`console error: ${m.text()}`);
      });
      await use();
      expect(problems, 'no page errors, console errors or outside requests').toEqual([]);
    },
    { auto: true },
  ],
  shot: async ({ page }, use, testInfo) => {
    await mkdir(SHOTS, { recursive: true });
    await use(async (name, opts) => {
      await opts?.show?.scrollIntoViewIfNeeded();
      if (opts?.settle !== false) await settle(page);
      const path = join(SHOTS, `${name}.png`);
      await page.screenshot({ path });
      await testInfo.attach(name, { path, contentType: 'image/png' });
    });
  },
});

export { expect };

/** Open a route (hash history) and check the page runs on the mock bridge, not inside Tauri. */
export async function open(page: Page, path: string): Promise<void> {
  await page.goto(`/#${path}`);
  await expect(page.locator('#app > *').first()).toBeVisible();
  const tauri = await page.evaluate(() => {
    const g = globalThis as { __TAURI__?: unknown; __TAURI_INTERNALS__?: unknown };
    return g.__TAURI__ !== undefined || g.__TAURI_INTERNALS__ !== undefined;
  });
  expect(tauri, 'E2E runs the browser build with the mock bridge').toBe(false);
}

/** Wait until every finite animation and transition has finished (infinite ones are ignored). */
export async function settle(page: Page): Promise<void> {
  await page.waitForFunction(() =>
    document
      .getAnimations()
      .every((a) => a.playState !== 'running' || a.effect?.getTiming().iterations === Infinity),
  );
}

/**
 * Click an SVG segment (an arc of the storage wheel) where it is really drawn: find a point
 * inside its fill that the browser hit-tests to the segment itself, then click there.
 */
export async function clickSegment(page: Page, seg: Locator, opts: { shift?: boolean } = {}): Promise<void> {
  await expect(seg).toBeVisible();
  const point = await seg.evaluate((el) => {
    const path = el as SVGPathElement;
    const box = path.getBBox();
    const ctm = path.getScreenCTM();
    if (!ctm) return null;
    const svg = path.ownerSVGElement!;
    const steps = [0.5, 0.4, 0.6, 0.3, 0.7, 0.2, 0.8, 0.1, 0.9];
    for (const fy of steps) {
      for (const fx of steps) {
        const p = svg.createSVGPoint();
        p.x = box.x + box.width * fx;
        p.y = box.y + box.height * fy;
        if (!path.isPointInFill(p)) continue;
        const s = p.matrixTransform(ctm);
        if (document.elementFromPoint(s.x, s.y) === path) return { x: s.x, y: s.y };
      }
    }
    return null;
  });
  expect(point, 'a clickable point inside the segment').not.toBeNull();
  if (opts.shift) await page.keyboard.down('Shift');
  await page.mouse.click(point!.x, point!.y);
  if (opts.shift) await page.keyboard.up('Shift');
}

/** Whether `loc` holds keyboard focus. */
export async function isFocused(loc: Locator): Promise<boolean> {
  return loc.evaluate((el) => el === document.activeElement).catch(() => false);
}

/** Press Tab (or Shift+Tab) until `target` has focus; fails after `max` presses. */
export async function tabTo(page: Page, target: Locator, opts: { back?: boolean; max?: number } = {}): Promise<void> {
  const key = opts.back ? 'Shift+Tab' : 'Tab';
  for (let i = 0; i < (opts.max ?? 80); i++) {
    if (await isFocused(target)) return;
    await page.keyboard.press(key);
  }
  expect(await isFocused(target), `reached ${String(target)} with ${key}`).toBe(true);
}

/** Running animations longer than 1 ms (reduced motion collapses every one to 1 ms, once). */
export async function runningAnimations(page: Page): Promise<string[]> {
  return page.evaluate(() =>
    document
      .getAnimations()
      .filter((a) => a.playState === 'running' && Number(a.effect?.getTiming().duration ?? 0) > 1)
      .map((a) => (a as CSSAnimation).animationName ?? a.constructor.name),
  );
}
