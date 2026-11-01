# Charts: Apache ECharts vs D3 + SVG (P3-CHART-01)

Decision D-6 left the chart library open until a benchmark. This is that benchmark. The
resulting decision is **D-37** (**provisional**; the maintainer
may overrule it).

## Recommendation

**Use D3's math modules with SVG rendered by Vue. Don't adopt ECharts.**

- Use `d3-scale`, `d3-shape` and `d3-hierarchy` (ISC) for scales, line and area paths, arcs,
  partitions and treemap layouts. Vue templates render the SVG. Leave out `d3-selection` and
  `d3-transition`: Vue owns the DOM, and motion is CSS that honors `prefers-reduced-motion`.
- Simple bars, meters and strips (regrowth, agent footprint, ecosystem mix, history budget)
  stay plain elements, as the design draws them. No chart library is needed for those.
- Every chart gets `role="img"` with a summary label, or labelled marks, and a visually
  hidden data table (PRD §14.4). Interactive charts get one tab stop with arrow-key
  navigation and an `aria-live` readout.
- Tiers use the existing SVG `<pattern>`s: violet solid, lilac solid, amber stripes, plum
  hatch. Colors come from `design/tokens.json` through CSS custom properties, so a later dark
  theme (D-16) is a stylesheet change.
- Hierarchy views (wheel, treemap) keep aggregating small items before rendering, using the
  Overview's `MIN_ITEM_DEG` rule (`src/components/overview/wheel.ts`). Neither library draws
  100K raw nodes interactively, and raw SVG is slowest of all in WebKit. Aggregated, a
  100K-leaf input becomes about 50 segments and D3 + SVG renders it in 28–103 ms.

This reverses the PRD §14.3 default ("Apache ECharts by default; D3.js only for hierarchy
views if ECharts fails the benchmark"). ECharts failed on several counts:

1. **Tier patterns.** Pattern support is a hard requirement (tiers are never color alone).
   ECharts 6.1 **ignores a per-item `itemStyle.decal` in the sunburst**: it only applies the
   automatic aria decal of each top-level node. Showing S2/S3 needed a workaround, an image
   pattern fill painted on a canvas tile.
2. **Accessibility.** ECharts' `aria` component adds one generated sentence to the container.
   For the regrowth bars it produced "the data for Week 1 is 0, 1.2, …", which mixes indices
   with values. It gave the stacked ecosystem chart no name at all. Marks aren't reachable
   and there is no keyboard support. With D3 + SVG every mark can carry a label, and the
   chart is one tab stop with arrow keys.
3. **Size.** About 194 KB gzip for the tree-shaken ECharts subset, against about 19 KB for
   the D3 modules (the D3 figure even includes `d3-selection`, which the app won't use). On
   top of that ECharts brings `tslib` (0BSD, not in our `deny.toml` allow list).
4. **Speed.** For every chart scull draws (Insights and the aggregated wheel and treemap),
   ECharts took 1.6–6.6× longer to first paint in both Chromium and WebKit, and was slower on
   hover.
   At 100K raw nodes nothing was interactive. Chromium favored SVG; WebKit favored ECharts'
   canvas (15 s vs 68 s for the wheel). Both point to aggregating first.
5. **Consistency.** The merged Overview wheel (D-19) is already hand-built SVG with these
   patterns, keyboard support and reduced motion. D3 + Vue SVG fits that code. ECharts would
   be a second rendering model.

What ECharts does better: a large built-in chart catalogue, built-in tooltips, legends and
zoom, and canvas rendering that keeps the DOM small. scull's charts are few and simple (design
05). Their requirements (patterns, ARIA, tokens, reduced motion) are where ECharts is weakest.

## What was measured

The benchmark lives in `apps/desktop/bench/`. It isn't shipped: nothing in `src/` imports it,
and the app's `vite build` never sees it. The same deterministic data is rendered by three
renderers:

| Page | Renderer |
|---|---|
| `d3` | d3-scale + d3-shape + d3-hierarchy for geometry, d3-selection writing SVG |
| `echarts-canvas` | ECharts 6.1 core + line/bar/sunburst/treemap + grid/tooltip/markLine/aria, canvas renderer |
| `echarts-svg` | The same, with the SVG renderer |
| `baseline` | Same harness and data, no chart: subtracted for bundle size and heap |

Scenarios:

- **insights**: the four Insights charts from design 05. The forecast line has a confidence
  band, cleanup markers and a 10 GB floor. The regrowth bars, the duplication bars and the
  ecosystem mix (stacked, ML with amber stripes) complete the set.
- **wheel-agg / treemap-agg**: a seeded tree of **100,000 leaves** in 8 categories (Pareto
  sizes, four tiers). It is aggregated with the app's rule (items under 2.5° fold into one
  "N smaller items" segment per category and tier), which leaves 46 nodes.
- **wheel-full / treemap-full**: the same 100,000 leaves, every one drawn (100,009 nodes).

Per page and scenario, in fresh browser contexts:

- **Bundle**: gzip -9 of every JS chunk the page loads, minus the baseline page.
- **Render**: the synchronous render call; the time to the first painted frame (rAF, then a
  task); the time since navigation (includes parsing the bundle and building the data); and
  the time until entry animations finish (`settled`).
- **Hover**: real mouse moves from Playwright over the chart (30 points, 10 for the 100K
  scenarios). The handler time runs from the `pointermove` capture to the `mousemove`
  bubble, which covers D3's pointer handler and zrender's mouse handler. The frame time runs
  from the event to the next painted frame. Reported as median (p95).
- **Re-theme**: swap to a dark palette and wait for the paint. D3 changes CSS custom
  properties. ECharts calls `setOption` with the new colors.
- **Heap**: Chromium only (CDP `JSHeapUsedSize` after a forced GC), minus the baseline.
- **Reduced motion**: the context emulates `prefers-reduced-motion: reduce`. The figure is
  the time until settled, plus whether CSS animations still run.
- **Accessibility**: Playwright's ARIA snapshot of the insights page and the count of
  keyboard-focusable chart elements.

Reproduce (Docker and the Playwright 1.64.1 image, like `scripts/evidence/web.sh`):

```bash
pnpm install --frozen-lockfile
PW_NODE_MODULES=<dir with playwright@1.64.1> node apps/desktop/bench/bench.mjs [--shots <dir>]
# BENCH_RUNS=5 BENCH_BROWSERS=chromium,webkit BENCH_SCENARIOS=insights,wheel-agg,... (defaults)
```

`bench.mjs` builds the pages, measures bundle sizes, runs `run.mjs` in the container, writes
`apps/desktop/bench/results.json` (summary plus raw runs) and prints the tables below.

## Results

Run on Oct 31, 2026 with echarts 6.1.0 / zrender 6.1.0 and d3-scale 4.0.2, d3-shape 3.2.0,
d3-hierarchy 3.1.2, d3-selection 3.0.0. Medians: 5 runs per page for insights and the aggregated
scenarios, 2 runs for the 100K scenarios. Raw runs are in `apps/desktop/bench/results.json`.

### Bundle size (gzip -9, tree-shaken, minus the shared harness and data)

| Page | Total gzip KB | Library gzip KB | Library raw KB |
|---|---:|---:|---:|
| baseline | 3.5 | 0.0 | 0.0 |
| d3 | 22.9 | 19.4 | 55.0 |
| echarts-canvas | 198.0 | 194.5 | 580.1 |
| echarts-svg | 201.2 | 197.7 | 586.1 |

### Chromium (Windows' WebView2 engine)

| Scenario | Library | Nodes | Render call ms | First frame ms | Since nav ms | Settled ms | Hover handler ms | Hover → frame ms (p95) | Re-theme ms | Extra heap MB | DOM nodes |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| insights | d3-svg | – | 27.2 | 52.5 | 154.4 | 412.7 | 0.30 | 2.7 (11.7) | 26.0 | 0.3 | 122 |
| insights | echarts-canvas | – | 186.9 | 217.9 | 379.0 | 1196.4 | 4.50 | 9.4 (46.3) | 59.7 | 3.3 | 30 |
| insights | echarts-svg | – | 168.9 | 194.5 | 374.8 | 1175.5 | 4.10 | 7.6 (26.7) | 49.9 | 3.4 | 128 |
| wheel-agg | d3-svg | 46 | 10.8 | 27.6 | 215.3 | 27.6 | 0.60 | 1.9 (6.2) | 4.8 | 0.3 | 75 |
| wheel-agg | echarts-canvas | 46 | 83.0 | 96.7 | 309.5 | 1067.1 | 2.10 | 8.0 (16.3) | 35.2 | 2.2 | 15 |
| wheel-agg | echarts-svg | 46 | 98.6 | 119.3 | 385.8 | 1075.0 | 2.00 | 8.4 (23.6) | 23.2 | 2.5 | 75 |
| wheel-full | d3-svg | 100009 | 2705.0 | 3762.2 | 3878.3 | 3762.2 | 23.90 | 162.6 (197.6) | 1693.8 | 24.8 | 100038 |
| wheel-full | echarts-canvas | 100009 | 14668.0 | 14814.4 | 15234.7 | 14814.4 | 93.80 | 1021.7 (1612.7) | 12472.7 | 219.4 | 15 |
| wheel-full | echarts-svg | 100009 | 14069.6 | 19066.7 | 19259.1 | 19066.7 | 70.25 | 4318.7 (6430.0) | 14013.8 | 244.9 | 100038 |
| treemap-agg | d3-svg | 46 | 9.1 | 41.5 | 321.0 | 41.5 | 0.10 | 0.7 (12.8) | 7.8 | 0.2 | 58 |
| treemap-agg | echarts-canvas | 46 | 85.4 | 115.1 | 657.6 | 115.1 | 2.85 | 11.9 (68.7) | 42.6 | 1.9 | 15 |
| treemap-agg | echarts-svg | 46 | 94.6 | 115.3 | 664.2 | 115.3 | 2.65 | 8.2 (76.0) | 44.1 | 2.1 | 97 |
| treemap-full | d3-svg | 100009 | 3247.6 | 3659.3 | 3855.6 | 3659.4 | 0.10 | 14.5 (224.4) | 1809.6 | 24.8 | 100021 |
| treemap-full | echarts-canvas | 100009 | 10757.3 | 10815.2 | 11157.6 | 10815.2 | 108.00 | 1323.3 (1670.7) | 11632.0 | 212.4 | 15 |
| treemap-full | echarts-svg | 100009 | 9052.8 | 14510.1 | 14700.5 | 14510.2 | 106.10 | 2465.3 (4601.7) | 8693.9 | 242.4 | 132097 |

### WebKit (macOS and Linux webview engine)

| Scenario | Library | Nodes | Render call ms | First frame ms | Since nav ms | Settled ms | Hover handler ms | Hover → frame ms (p95) | Re-theme ms | Extra heap MB | DOM nodes |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| insights | d3-svg | – | 60.0 | 153.0 | 222.0 | 439.0 | 1.00 | 13.0 (37.0) | 15.0 | – | 122 |
| insights | echarts-canvas | – | 293.0 | 754.0 | 961.0 | 1293.0 | 10.00 | 35.0 (127.0) | 163.0 | – | 30 |
| insights | echarts-svg | – | 327.0 | 397.0 | 565.0 | 1411.0 | 7.50 | 18.5 (54.0) | 85.0 | – | 128 |
| wheel-agg | d3-svg | 46 | 31.0 | 75.0 | 181.0 | 75.0 | 1.00 | 29.0 (87.0) | 36.0 | – | 75 |
| wheel-agg | echarts-canvas | 46 | 183.0 | 498.0 | 838.0 | 1371.0 | 7.00 | 112.0 (505.0) | 207.0 | – | 15 |
| wheel-agg | echarts-svg | 46 | 195.0 | 363.0 | 704.0 | 1300.0 | 5.00 | 78.5 (302.0) | 65.0 | – | 75 |
| treemap-agg | d3-svg | 46 | 32.0 | 103.0 | 248.0 | 103.0 | 0.00 | 17.0 (107.0) | 54.0 | – | 58 |
| treemap-agg | echarts-canvas | 46 | 100.0 | 168.0 | 377.0 | 168.0 | 5.00 | 24.0 (86.0) | 54.0 | – | 15 |
| treemap-agg | echarts-svg | 46 | 153.0 | 172.0 | 395.0 | 172.0 | 6.00 | 30.0 (134.0) | 83.0 | – | 97 |
| wheel-full | d3-svg | 100009 | 4762.0 | 68174.0 | 68331.0 | 68174.0 | – | > 60 s (timed out) | 112194.0 | – | 100038 |
| wheel-full | echarts-canvas | 100009 | 13118.5 | 14925.0 | 15570.0 | 14925.0 | 91.50 | 1525.0 (2496.0) | 13856.0 | – | 15 |
| wheel-full | echarts-svg | 100009 | 17674.5 | 92180.0 | 92839.0 | 92180.0 | – | > 60 s (timed out) | 13104.0 | – | 100038 |
| treemap-full | d3-svg | 100009 | 5765.0 | 23027.5 | 23247.5 | 23027.5 | 0.00 | 3537.5 (20637.0) | 13252.0 | – | 100021 |
| treemap-full | echarts-canvas | 100009 | 8275.5 | 8969.0 | 9397.0 | 8969.0 | 105.50 | 1557.0 (2801.0) | 13618.5 | – | 15 |
| treemap-full | echarts-svg | 100009 | 10118.0 | 29700.0 | 29970.0 | 29700.0 | 162.50 | 13799.0 (30795.0) | 9271.0 | – | 132097 |

"Settled" includes entry animations: ECharts animates for about 1 s by default; the D3 page
uses a 340 ms CSS rise. "> 60 s (timed out)": the first hover hadn't painted after 60 s, and
the run moved on. Heap is Chromium-only (CDP).

### What the numbers say

- **The charts scull actually draws** (the Insights tiles, an aggregated wheel or treemap): D3 +
  SVG reaches its first frame 2.8–4.3× sooner than ECharts in Chromium (28–53 ms vs
  97–218 ms) and 1.6–6.6× sooner in WebKit (75–153 ms vs 168–754 ms). Its hover-to-paint
  latency is lower in every case, re-theming is cheaper or equal, and it uses about a tenth of
  the extra heap (0.2–0.4 MB vs 1.9–3.4 MB).
- **100K raw nodes.** No renderer is usable interactively:
  - In Chromium, SVG draws first (3.7 s vs 11–19 s for ECharts) and hovers at about 160 ms.
    ECharts takes 1–4 s per hover, plus 200+ MB of heap.
  - In **WebKit, raw SVG is the slowest**: the 100K-arc wheel takes 68 s to its first frame
    (ECharts canvas 15 s), and its hovers time out. ECharts canvas hovers take about 1.5 s.
  - The fix is the one the Overview already applies: aggregate first. With the app's 2.5° rule,
    100K leaves become 46 segments, and every library is fast.
- **Bundle.** The D3 modules cost about 19 KB gzip (`d3-selection` included; the app won't use
  it). ECharts costs about 194 KB gzip for the subset used here.

### Accessibility, patterns, motion, theming

| | D3 + SVG (Vue) | ECharts canvas | ECharts SVG |
|---|---|---|---|
| Screen-reader structure | Each chart is a labelled group; every mark is a labelled list item ("May: ML / AI 12 GB, JavaScript 41 GB, …"); a hidden table is easy to add | One `img` per chart with a generated sentence. Regrowth: "the data for Week 1 is 0, 1.2, …" (indices mixed with values). The stacked mix chart got **no name**. | Same as canvas; the SVG nodes add no semantics |
| Keyboard | Forecast is one tab stop with arrow keys and a live readout (1 focusable element) | None (0 focusable) | None (0) |
| Tier patterns (color AND pattern) | SVG `<pattern>` (stripes, hatch) on any mark | `itemStyle.decal` works on bars, but the **sunburst ignores per-item decals**. Needed a canvas-tile image fill | Same as canvas |
| Reduced motion | CSS `@media (prefers-reduced-motion)`: settled 42 ms vs 413 ms (Chromium) | `animation: false` from `matchMedia` in JS: settled 206 ms vs 1196 ms | 137 ms vs 1176 ms |
| Theming from `design/tokens.json` | CSS custom properties: a theme switch is a stylesheet change (re-theme 5–26 ms) | Resolved colors through `setOption` (35–60 ms; seconds at 100K) | Same (23–50 ms) |

Screenshots of every page in both engines are attached to issue #105.

## Licenses

| Package | License | Notes |
|---|---|---|
| d3-scale, d3-shape, d3-hierarchy (+ d3-array, d3-color, d3-format, d3-interpolate, d3-path, d3-time, d3-time-format, internmap) | ISC | All allowed by `deny.toml`'s list (ISC, MIT, Apache-2.0, BSD). |
| d3-selection | ISC | Benchmark only; the app doesn't need it. |
| echarts 6.1.0 | Apache-2.0 | Allowed. |
| zrender 6.1.0 | BSD-3-Clause | Allowed. |
| tslib 2.3.0 (ECharts runtime dependency, bundled) | 0BSD | Permissive, but **not** in the allow list; adopting ECharts would need `0BSD` added. |

`cargo-deny` covers only Rust crates. For npm the same allow list was checked by hand from
each package's `license` field (`apps/desktop/node_modules/.pnpm/*/package.json`). All the
benchmark packages are `devDependencies`. P3-UI-10 moves `d3-scale` and `d3-shape` to
`dependencies` when the Insights screen uses them.

## Caveats

- The numbers come from headless browsers in Docker with software rendering (SwiftShader) on
  a shared 18-vCPU VM under heavy load from other jobs. Compare libraries within a run, not
  absolute values across machines. The ratios were stable across runs.
- Tauri uses WebKit on macOS and Linux and WebView2 (Chromium) on Windows. Both engines were
  measured; Playwright's WebKit build stands in for WKWebView and WebKitGTK.
- The D3 page uses `d3-selection`, the idiomatic way to write SVG with D3. The recommended
  app pattern (Vue templates) adds no library code. Its render cost is Vue's patching of the
  same SVG elements.
