// Playwright driver for the chart benchmark. Runs inside mcr.microsoft.com/playwright (started
// by bench/bench.mjs): serves bench/dist, loads every page × scenario in fresh browser contexts,
// and writes raw measurements as JSON.
//
//   node run.mjs <distDir> <out.json> [shotsDir]
// Env: BENCH_RUNS (default 5; at most 2 for the 100K-node scenarios), BENCH_BROWSERS (default "chromium,webkit"), BENCH_SCENARIOS.
import { createServer } from 'node:http';
import { readFile, writeFile, mkdir } from 'node:fs/promises';
import { extname, join, normalize } from 'node:path';
import { chromium, webkit } from 'playwright';

const [dist, outFile, shots] = process.argv.slice(2);
const RUNS = Number(process.env.BENCH_RUNS ?? 5);
const BROWSERS = (process.env.BENCH_BROWSERS ?? 'chromium,webkit').split(',');
const SCENARIOS = (process.env.BENCH_SCENARIOS ?? 'insights,wheel-agg,wheel-full,treemap-agg,treemap-full').split(',');
const PAGES = ['baseline', 'd3', 'echarts-canvas', 'echarts-svg'];
// 100K-node scenarios can take seconds per hover with some renderers: fewer samples there.
const hoverCount = (scenario) => (scenario.endsWith('-full') ? 5 : 30);
/** A page that hasn't rendered after this long is recorded as timed out (not retried). */
const RENDER_TIMEOUT = 300_000;
const HOVER_TIMEOUT = 60_000;
const TYPES = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.json': 'application/json' };

const server = createServer(async (req, res) => {
  const path = normalize(decodeURIComponent(new URL(req.url, 'http://x').pathname)).replace(/^(\.\.[/\\])+/, '');
  try {
    const body = await readFile(join(dist, path));
    res.writeHead(200, { 'content-type': TYPES[extname(path)] ?? 'application/octet-stream' });
    res.end(body);
  } catch {
    res.writeHead(404);
    res.end();
  }
});
await new Promise((r) => server.listen(0, '127.0.0.1', r));
const origin = `http://127.0.0.1:${server.address().port}`;
if (shots) await mkdir(shots, { recursive: true });

/** Points to hover over, inside the chart's box. */
function hoverPath(scenario, b) {
  const n = hoverCount(scenario);
  return Array.from({ length: n }, (_, i) => {
    const f = i / (n - 1);
    if (scenario.startsWith('wheel')) {
      const a = f * 2 * Math.PI;
      const r = (227 / 620) * b.width;
      return [b.x + b.width / 2 + r * Math.sin(a), b.y + b.height / 2 - r * Math.cos(a)];
    }
    if (scenario.startsWith('treemap')) return [b.x + 4 + f * (b.width - 8), b.y + 4 + f * (b.height - 8)];
    return [b.x + 0.03 * b.width + f * 0.94 * b.width, b.y + b.height * 0.5];
  });
}

async function measure(browser, browserName, lib, scenario, run, reduced) {
  const context = await browser.newContext({
    viewport: { width: 1440, height: 1000 },
    deviceScaleFactor: 1,
    reducedMotion: reduced ? 'reduce' : 'no-preference',
  });
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  await page.goto(`${origin}/${lib}.html?scenario=${scenario}`);
  try {
    await page.waitForFunction(() => window.__bench !== undefined, null, { timeout: RENDER_TIMEOUT, polling: 500 });
  } catch {
    await context.close();
    return { browser: browserName, lib, scenario, run, reduced, timedOut: true, hovers: [], errors };
  }
  const r = { browser: browserName, run, ...(await page.evaluate(() => ({ ...window.__bench }))) };

  if (browserName === 'chromium') {
    const cdp = await context.newCDPSession(page);
    await cdp.send('HeapProfiler.collectGarbage');
    await cdp.send('Performance.enable');
    const { metrics } = await cdp.send('Performance.getMetrics');
    const m = Object.fromEntries(metrics.map((x) => [x.name, x.value]));
    r.jsHeapMB = m.JSHeapUsedSize / 2 ** 20;
    r.cdpNodes = m.Nodes;
  }

  if (lib !== 'baseline') {
    const box = await page.locator('[data-bench-hover]').boundingBox();
    let n = 0;
    for (const [x, y] of hoverPath(scenario, box)) {
      await page.mouse.move(x, y, { timeout: HOVER_TIMEOUT }).catch(() => (r.hoverTimedOut = true));
      n += 1;
      // A hover that hasn't painted within a minute is recorded, and the remaining ones skipped.
      await page
        .waitForFunction((k) => window.__bench.hovers.length >= k, n, { timeout: HOVER_TIMEOUT, polling: 250 })
        .catch(() => (r.hoverTimedOut = true));
      if (r.hoverTimedOut) break;
    }
    r.hovers = await page.evaluate(() => window.__bench.hovers);
    if (shots && run === 0 && !reduced) await page.screenshot({ path: join(shots, `${browserName}-${lib}-${scenario}.png`), fullPage: true, timeout: HOVER_TIMEOUT }).catch(() => undefined);
    r.rethemeMs = await Promise.race([
      page.evaluate(() => window.__benchRetheme()),
      new Promise((resolve) => setTimeout(() => resolve(null), 2 * HOVER_TIMEOUT)),
    ]);
    if (scenario === 'insights' && run === 0) {
      r.runningAnimations = await page.evaluate(() => document.getAnimations().filter((a) => a.playState === 'running').length);
      r.focusable = await page.locator('#root [tabindex="0"], #root button, #root a[href]').count();
      r.aria = await page.locator('#root').ariaSnapshot();
    }
  }
  r.errors = errors;
  await context.close();
  return r;
}

// Results are written after every page, so an interrupted run keeps what it measured.
const results = [];
for (const name of BROWSERS) {
  const browser = await (name === 'webkit' ? webkit : chromium).launch();
  for (const scenario of SCENARIOS) {
    for (const lib of PAGES) {
      const runs = scenario.endsWith('-full') ? Math.min(RUNS, 2) : RUNS;
      for (let run = 0; run < runs; run++) {
        const r = await measure(browser, name, lib, scenario, run, false);
        results.push(r);
        await writeFile(outFile, JSON.stringify(results));
        process.stderr.write(`${name} ${scenario} ${lib} #${run}: ${r.timedOut ? 'timed out' : `frame ${r.firstFrameMs.toFixed(1)} ms`}\n`);
        if (r.timedOut) break;
      }
      if (scenario === 'insights' && lib !== 'baseline') results.push(await measure(browser, name, lib, scenario, 0, true));
    }
  }
  await browser.close();
}
await writeFile(outFile, JSON.stringify(results));
server.close();
