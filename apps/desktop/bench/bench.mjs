// Chart benchmark (P3-CHART-01, decision D-6/D-37): ECharts vs D3 + SVG.
//
//   node apps/desktop/bench/bench.mjs [--skip-run] [--keep] [--shots <dir>]
//
// 1. Builds the benchmark pages (bench/vite.config.ts) and measures gzip sizes per page.
// 2. Runs bench/run.mjs in the Playwright container (Chromium + WebKit) for render, hover,
//    memory, re-theme, reduced-motion and accessibility measurements.
// 3. Writes bench/results.json (summary + raw) and prints the tables used in docs/CHARTS.md.
//
// Needs docker, the image mcr.microsoft.com/playwright:v1.64.1-noble and a node_modules dir with
// playwright@1.64.1 (PW_NODE_MODULES, default: scripts/evidence/node_modules), like
// scripts/evidence/web.sh. Results vary by machine; compare libraries within one run.
import { execFileSync } from 'node:child_process';
import { readFileSync, writeFileSync, mkdtempSync, existsSync, mkdirSync, realpathSync } from 'node:fs';
import { gzipSync } from 'node:zlib';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const desktop = resolve(here, '..');
const repo = resolve(desktop, '../..');
const dist = join(here, 'dist');
const args = process.argv.slice(2);
const skipRun = args.includes('--skip-run');
const shotsArg = args.includes('--shots') ? resolve(args[args.indexOf('--shots') + 1]) : null;
const PAGES = ['baseline', 'd3', 'echarts-canvas', 'echarts-svg'];

// ---- 1. Build + bundle sizes ----
execFileSync('pnpm', ['exec', 'vite', 'build', '--config', 'bench/vite.config.ts'], { cwd: desktop, stdio: 'inherit' });
const manifest = JSON.parse(readFileSync(join(dist, '.vite/manifest.json'), 'utf8'));

function files(key, seen = new Set()) {
  const e = manifest[key];
  if (!e || seen.has(key)) return seen;
  seen.add(key);
  for (const i of e.imports ?? []) files(i, seen);
  return seen;
}
const bundle = {};
for (const p of PAGES) {
  const keys = files(`${p}.html`);
  let raw = 0;
  let gz = 0;
  for (const k of keys) {
    const buf = readFileSync(join(dist, manifest[k].file));
    raw += buf.length;
    gz += gzipSync(buf, { level: 9 }).length;
  }
  bundle[p] = { rawKB: raw / 1024, gzipKB: gz / 1024 };
}
for (const p of PAGES) {
  bundle[p].libGzipKB = bundle[p].gzipKB - bundle.baseline.gzipKB;
  bundle[p].libRawKB = bundle[p].rawKB - bundle.baseline.rawKB;
}

// ---- 2. Browser runs ----
let raw;
const resultsFile = join(here, 'results.json');
if (skipRun) {
  raw = JSON.parse(readFileSync(resultsFile, 'utf8')).raw;
} else {
  const out = mkdtempSync(join(tmpdir(), 'scull-bench-'));
  const mods = process.env.PW_NODE_MODULES ?? join(repo, 'scripts/evidence/node_modules');
  if (shotsArg && !existsSync(shotsArg)) mkdirSync(shotsArg, { recursive: true });
  const env = ['BENCH_RUNS', 'BENCH_BROWSERS', 'BENCH_SCENARIOS'].flatMap((k) => (process.env[k] ? ['-e', `${k}=${process.env[k]}`] : []));
  // A failed container still leaves the measurements it wrote.
  try {
    execFileSync(
    'docker',
    [
      'run', '--rm', '--ipc=host', '--user', `${process.getuid()}:${process.getgid()}`, '-e', 'HOME=/tmp', ...env,
      '-v', `${dist}:/dist:ro`, '-v', `${out}:/out`, '-v', `${mods}:/pw/node_modules:ro`,
      '-v', `${join(here, 'run.mjs')}:/pw/run.mjs:ro`,
      ...(shotsArg ? ['-v', `${shotsArg}:/shots`] : []),
      '-w', '/pw', 'mcr.microsoft.com/playwright:v1.64.1-noble',
      'node', '/pw/run.mjs', '/dist', '/out/raw.json', ...(shotsArg ? ['/shots'] : []),
    ],
    { stdio: 'inherit' },
  );
  } catch (e) {
    console.error(`benchmark run failed: ${e.message.split('\n')[0]}`);
  }
  raw = JSON.parse(readFileSync(join(out, 'raw.json'), 'utf8'));
  // --keep: keep earlier results of the browser × scenario pairs this run didn't measure.
  if (args.includes('--keep') && existsSync(resultsFile)) {
    const ran = new Set(raw.map((r) => `${r.browser}|${r.scenario}`));
    raw = [...JSON.parse(readFileSync(resultsFile, 'utf8')).raw.filter((r) => !ran.has(`${r.browser}|${r.scenario}`)), ...raw];
  }
}

// ---- 3. Summary ----
const median = (xs) => {
  const s = xs.filter((x) => Number.isFinite(x)).sort((a, b) => a - b);
  if (!s.length) return null;
  const m = Math.floor(s.length / 2);
  return s.length % 2 ? s[m] : (s[m - 1] + s[m]) / 2;
};
const p95 = (xs) => {
  const s = xs.filter((x) => Number.isFinite(x)).sort((a, b) => a - b);
  return s.length ? s[Math.min(s.length - 1, Math.floor(s.length * 0.95))] : null;
};

const summary = [];
const groups = new Map();
for (const r of raw) {
  const k = `${r.browser}|${r.scenario}|${r.lib}|${r.reduced}`;
  if (!groups.has(k)) groups.set(k, []);
  groups.get(k).push(r);
}
for (let [k, rs] of groups) {
  const [browser, scenario, lib, reduced] = k.split('|');
  const base = groups.get(`${browser}|${scenario}|baseline|false`) ?? [];
  const timedOut = rs.filter((r) => r.timedOut).length;
  rs = rs.filter((r) => !r.timedOut);
  if (rs.length === 0) {
    summary.push({ browser, scenario, lib, reduced: reduced === 'true', runs: 0, timedOut, errors: [] });
    continue;
  }
  const hovers = rs.flatMap((r) => r.hovers ?? []);
  const heap = median(rs.map((r) => r.jsHeapMB));
  const baseHeap = median(base.map((r) => r.jsHeapMB));
  summary.push({
    browser,
    scenario,
    lib,
    reduced: reduced === 'true',
    runs: rs.length,
    timedOut,
    nodes: rs[0].nodes,
    renderCallMs: median(rs.map((r) => r.renderCallMs)),
    firstFrameMs: median(rs.map((r) => r.firstFrameMs)),
    sinceNavMs: median(rs.map((r) => r.sinceNavMs)),
    settledMs: median(rs.map((r) => r.settledMs)),
    hoverHandlerMs: median(hovers.map((h) => h.handlerMs)),
    hoverFrameMs: median(hovers.map((h) => h.frameMs)),
    hoverFrameP95Ms: p95(hovers.map((h) => h.frameMs)),
    hoverTimedOut: rs.some((r) => r.hoverTimedOut),
    rethemeMs: median(rs.map((r) => r.rethemeMs)),
    jsHeapMB: heap,
    extraHeapMB: heap !== null && baseHeap !== null ? heap - baseHeap : null,
    domNodes: median(rs.map((r) => r.domNodes)),
    focusable: rs.find((r) => r.focusable !== undefined)?.focusable,
    runningAnimations: rs.find((r) => r.runningAnimations !== undefined)?.runningAnimations,
    aria: rs.find((r) => r.aria !== undefined)?.aria,
    errors: [...new Set(rs.flatMap((r) => r.errors ?? []))],
  });
}

const versions = Object.fromEntries(
  ['echarts', 'zrender', 'd3-scale', 'd3-shape', 'd3-hierarchy', 'd3-selection'].map((n) => {
    // zrender is ECharts' own dependency: read it next to echarts in pnpm's store.
    const dir = n === 'zrender' ? join(realpathSync(join(desktop, 'node_modules/echarts')), '../zrender') : join(desktop, 'node_modules', n);
    const pj = JSON.parse(readFileSync(join(dir, 'package.json'), 'utf8'));
    return [n, { version: pj.version, license: pj.license }];
  }),
);
writeFileSync(
  resultsFile,
  `${JSON.stringify({ date: new Date().toISOString().slice(0, 10), versions, bundle, summary, raw }, null, 1)}\n`,
);

// ---- Tables ----
const f = (x, d = 1) => (x === null || x === undefined ? '–' : x.toFixed(d));
console.log('\n### Bundle (gzip -9, tree-shaken)\n\n| Page | Total gzip KB | Library gzip KB | Library raw KB |\n|---|---:|---:|---:|');
for (const p of PAGES) console.log(`| ${p} | ${f(bundle[p].gzipKB)} | ${f(bundle[p].libGzipKB)} | ${f(bundle[p].libRawKB)} |`);
for (const browser of [...new Set(summary.map((s) => s.browser))]) {
  console.log(
    `\n### ${browser}\n\n| Scenario | Library | Nodes | Render call ms | First frame ms | Since nav ms | Settled ms | Hover handler ms | Hover → frame ms (p95) | Re-theme ms | Extra heap MB | DOM nodes |\n|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|`,
  );
  for (const s of summary.filter((x) => x.browser === browser && !x.reduced && x.lib !== 'baseline')) {
    if (s.runs === 0) {
      console.log(`| ${s.scenario} | ${s.lib} | 100009 | timed out (no frame within 300 s) | | | | | | | | |`);
      continue;
    }
    console.log(
      `| ${s.scenario} | ${s.lib} | ${s.nodes || '–'} | ${f(s.renderCallMs)} | ${f(s.firstFrameMs)} | ${f(s.sinceNavMs)} | ${f(s.settledMs)} | ${f(s.hoverHandlerMs, 2)} | ${s.hoverTimedOut ? '> 60 s (timed out)' : `${f(s.hoverFrameMs)} (${f(s.hoverFrameP95Ms)})`} | ${f(s.rethemeMs)} | ${f(s.extraHeapMB)} | ${f(s.domNodes, 0)} |`,
    );
  }
  console.log('\nReduced motion (insights): settled ms with / without, running CSS animations');
  for (const s of summary.filter((x) => x.browser === browser && x.reduced)) {
    const normal = summary.find((x) => x.browser === browser && x.scenario === s.scenario && x.lib === s.lib && !x.reduced);
    console.log(`- ${s.lib}: ${f(s.settledMs)} / ${f(normal?.settledMs)} ms, running animations: ${s.runningAnimations}`);
  }
  const errs = summary.filter((x) => x.browser === browser && x.errors.length);
  for (const s of errs) console.log(`! ${s.scenario} ${s.lib}: ${s.errors.join('; ')}`);
}
