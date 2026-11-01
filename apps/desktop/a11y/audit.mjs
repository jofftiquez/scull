// Accessibility audit of every desktop route (P3-A11Y-01, WCAG 2.2 AA). Runs inside the
// Playwright container against the Vite dev server (mock bridge), started by a11y/audit.sh.
//
//   node audit.mjs <baseUrl> <out.json> [shotsDir]
//
// Per route: axe-core (WCAG 2.0/2.1/2.2 A+AA rules and best practices), a 44 px target-size
// check (scull's own bar, stricter than WCAG 2.5.8's 24 px), a keyboard pass (Tab order, a
// visible focus indicator on every stop, no trap), and the same page under
// `prefers-reduced-motion: reduce` (no finite animation may still be running).
//
// axe-core is MPL-2.0, so it is NOT a dependency of this repository: like Playwright it is
// loaded from a tooling directory mounted into the container (AXE_NODE_MODULES) and never
// bundled or shipped.
import { readFile, writeFile, mkdir } from 'node:fs/promises';
import { join } from 'node:path';
import { chromium } from 'playwright';

const [base, outFile, shots] = process.argv.slice(2);
const axeSource = await readFile('/axe/node_modules/axe-core/axe.min.js', 'utf8');
if (shots) await mkdir(shots, { recursive: true });

/** Routes and states; `ready` is a selector that exists once the screen has its data. */
export const ROUTES = [
  { path: '/', ready: '[data-testid="header"]' },
  { path: '/projects', ready: 'h1' },
  { path: '/projects?demo=design', ready: 'h1' },
  { path: '/explorer', ready: 'main' },
  { path: '/worktrees', ready: 'h1' },
  { path: '/caches', ready: 'main' },
  { path: '/cleanup', ready: 'main' },
  { path: '/insights', ready: '[data-testid="forecast-chart"]' },
  { path: '/insights?demo=design', ready: '[data-testid="forecast-chart"]' },
  { path: '/history', ready: 'main' },
  { path: '/settings', ready: 'main' },
  { path: '/onboarding', ready: 'h1' },
];

const TAGS = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa', 'best-practice'];
const MAX_TABS = 120;

/** Wait until the DOM has been quiet for 400 ms and finite animations are done. */
async function settle(page) {
  await page.evaluate(
    () =>
      new Promise((resolve) => {
        let t = setTimeout(done, 400);
        const mo = new MutationObserver(() => {
          clearTimeout(t);
          t = setTimeout(done, 400);
        });
        mo.observe(document.body, { subtree: true, childList: true, attributes: true, characterData: true });
        function done() {
          mo.disconnect();
          resolve();
        }
      }),
  );
  await page.evaluate(() =>
    Promise.all(
      document
        .getAnimations()
        .filter((a) => a.effect?.getTiming().iterations !== Infinity)
        .map((a) => a.finished.catch(() => undefined)),
    ),
  );
}

async function open(browser, path, reduced) {
  const context = await browser.newContext({
    viewport: { width: 1440, height: 900 },
    reducedMotion: reduced ? 'reduce' : 'no-preference',
  });
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  await page.goto(`${base}/#${path}`);
  return { context, page, errors };
}

function describe() {
  // Runs in the page: a short, stable description of the focused element.
  const el = document.activeElement;
  if (!el || el === document.body) return null;
  const name = (el.getAttribute('aria-label') || el.textContent || '').trim().replace(/\s+/g, ' ').slice(0, 60);
  const r = el.getBoundingClientRect();
  const cs = getComputedStyle(el);
  const ring = (c) => (c.outlineStyle !== 'none' && parseFloat(c.outlineWidth) > 0) || (c.boxShadow && c.boxShadow !== 'none');
  // SVG marks show focus with a stroke; a field may show it on its wrapper (:focus-within).
  let wrapper = false;
  for (let p = el.parentElement, i = 0; p && i < 3; p = p.parentElement, i++) {
    if (p.matches(':focus-within') && ring(getComputedStyle(p))) wrapper = true;
  }
  // SVG marks: a stroke on the mark or on a focus shape inside it (e.g. a treemap tile's ring).
  const stroked = (c) => c.stroke !== 'none' && !/rgba\(0, 0, 0, 0\)|transparent/.test(c.stroke) && parseFloat(c.strokeWidth) > 0;
  const svgRing = el instanceof SVGElement && [el, ...el.querySelectorAll('.focus, [data-focus-ring]')].some((n) => stroked(getComputedStyle(n)));
  const visible = ring(cs) || svgRing || wrapper;
  return {
    tag: el.tagName.toLowerCase(),
    role: el.getAttribute('role'),
    name,
    focusVisible: el.matches(':focus-visible'),
    indicator: Boolean(visible),
    w: Math.round(r.width),
    h: Math.round(r.height),
  };
}

const withoutKey = (s) => Object.fromEntries(Object.entries(s).filter(([k]) => k !== 'key'));

async function audit(browser, route) {
  const { context, page, errors } = await open(browser, route.path, false);
  await page.waitForSelector(route.ready, { timeout: 30_000 });
  await settle(page);

  await page.addScriptTag({ content: axeSource });
  const axe = await page.evaluate(
    (tags) =>
      window.axe.run(document, { runOnly: { type: 'tag', values: tags }, resultTypes: ['violations', 'incomplete'] }),
    TAGS,
  );
  const violations = axe.violations.map((v) => ({
    id: v.id,
    impact: v.impact,
    help: v.help,
    tags: v.tags.filter((t) => t.startsWith('wcag')),
    nodes: v.nodes.map((n) => ({ target: n.target.join(' '), summary: n.failureSummary?.split('\n').slice(0, 3).join(' ') })),
  }));
  // "Incomplete": axe couldn't decide (e.g. text over a gradient); listed for a manual check.
  const incomplete = axe.incomplete.map((v) => ({
    id: v.id,
    nodes: v.nodes.map((n) => ({ target: n.target.join(' '), why: n.any?.[0]?.message ?? '' })),
  }));

  // Targets: every visible interactive element, smaller than 44 × 44 px.
  const small = await page.evaluate(() => {
    const out = [];
    const els = document.querySelectorAll('button, a[href], input, select, textarea, summary, [role="button"], [role="tab"], [role="switch"], [tabindex="0"]');
    for (const el of els) {
      if (el.disabled) continue; // not a target while disabled
      // A control inside a <label> is hit through the whole label.
      const hit = el.matches('input, select, textarea') && el.closest('label') ? el.closest('label') : el;
      const box = hit.getBoundingClientRect();
      if (box.width === 0 || box.height === 0) continue;
      // An absolutely positioned ::before/::after with negative insets enlarges the hit area.
      const grow = (v) => Math.max(0, -parseFloat(v) || 0);
      const r = { width: box.width, height: box.height };
      for (const which of ['::before', '::after']) {
        const pb = getComputedStyle(hit, which);
        if (pb.content === 'none' || pb.position !== 'absolute') continue;
        r.width = Math.max(r.width, box.width + grow(pb.left) + grow(pb.right));
        r.height = Math.max(r.height, box.height + grow(pb.top) + grow(pb.bottom));
      }
      if (getComputedStyle(el).visibility === 'hidden') continue;
      if (el.closest('.sr-only')) continue;
      if (r.width < 44 || r.height < 44) {
        const name = (el.getAttribute('aria-label') || el.textContent || '').trim().replace(/\s+/g, ' ').slice(0, 50);
        // Inline links inside running text are exempt (WCAG 2.5.8 "inline").
        const inline = el.tagName === 'A' && getComputedStyle(el).display === 'inline';
        out.push({ tag: el.tagName.toLowerCase(), name, w: Math.round(r.width), h: Math.round(r.height), inline });
      }
    }
    return out;
  });

  // Keyboard: Tab through the page from the top.
  await page.evaluate(() => {
    document.activeElement?.blur?.();
    window.scrollTo(0, 0);
  });
  const stops = [];
  for (let i = 0; i < MAX_TABS; i++) {
    await page.keyboard.press('Tab');
    const d = await page.evaluate(describe);
    if (!d) break;
    const key = `${d.tag}|${d.role}|${d.name}`;
    if (stops.length > 0 && key === stops[0].key) break; // wrapped around
    stops.push({ key, ...d });
  }
  const noIndicator = stops.filter((s) => !s.indicator);

  if (shots) {
    const name = route.path.replace(/[^a-z0-9]+/gi, '-').replace(/^-|-$/g, '') || 'overview';
    await page.screenshot({ path: join(shots, `${name}.png`) });
  }
  await context.close();

  // Reduced motion: load again and check nothing finite is still animating once settled.
  const rm = await open(browser, route.path, true);
  await rm.page.waitForSelector(route.ready, { timeout: 30_000 });
  const motion = await rm.page.evaluate(() =>
    document
      .getAnimations()
      .map((a) => ({
        name: a.animationName ?? a.constructor.name,
        duration: Number(a.effect?.getTiming().duration ?? 0),
        iterations: a.effect?.getTiming().iterations,
        state: a.playState,
      }))
      .filter((a) => a.state === 'running' && a.duration > 1),
  );
  await rm.context.close();

  return {
    path: route.path,
    violations,
    incomplete,
    smallTargets: small,
    tabStops: stops.length,
    tabOrder: stops.map(withoutKey),
    noFocusIndicator: noIndicator.map(withoutKey),
    reducedMotionRunning: motion,
    errors: [...errors, ...rm.errors],
  };
}

const browser = await chromium.launch();
const results = [];
for (const route of ROUTES) {
  const r = await audit(browser, route);
  results.push(r);
  process.stderr.write(
    `${route.path}: ${r.violations.length} axe violation(s), ${r.smallTargets.filter((t) => !t.inline).length} small target(s), ` +
      `${r.tabStops} tab stops (${r.noFocusIndicator.length} without indicator), ${r.reducedMotionRunning.length} animation(s) under reduced motion\n`,
  );
}
await browser.close();
await writeFile(outFile, JSON.stringify(results, null, 1));
