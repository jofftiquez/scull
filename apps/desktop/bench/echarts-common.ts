// Apache ECharts (Apache-2.0) renderer, tree-shaken to the charts the Insights screen and the
// wheel/treemap need. The page files register exactly one zrender renderer (canvas or SVG) so
// each bundle carries only what it uses. Tier patterns use ECharts' `decal`; accessibility uses
// the `aria` component (a generated description on the container).
import * as echarts from 'echarts/core';
import type { EChartsCoreOption, EChartsType } from 'echarts/core';
import { BarChart, LineChart, SunburstChart, TreemapChart } from 'echarts/charts';
import { AriaComponent, GridComponent, MarkLineComponent, TooltipComponent } from 'echarts/components';
import { DUPLICATION, ECOSYSTEMS, ECOSYSTEM_LABEL, ECOSYSTEM_MIX, FORECAST, REGROWTH, type Leaf, type Tier, type Tree } from './data';
import { C, DARK, ECO_COLOR, TIER_LABEL, TIER_PATTERN, TIER_SOLID } from './theme';
import { nextFrame, type Handle, type Renderer } from './harness';

echarts.use([LineChart, BarChart, SunburstChart, TreemapChart, GridComponent, TooltipComponent, MarkLineComponent, AriaComponent]);

export { echarts };

type Palette = typeof C;

/** Stripes (S2, ML series) and hatch (S3) as ECharts decals over the solid color. */
function decal(color: string, gap: number) {
  return { symbol: 'rect', symbolSize: 1, dashArrayX: [1, 0], dashArrayY: [gap, gap], rotation: -Math.PI / 4, color };
}
/**
 * Tier fill for the sunburst/treemap. ECharts 6.1 ignores a per-item `itemStyle.decal` in the
 * sunburst (it only applies the aria auto-decal per top-level node), so S2/S3 use an image
 * pattern fill: a small canvas tile with the stripes/hatch.
 */
const tiles = new Map<Tier, HTMLCanvasElement>();
function patternTile(t: Tier, base: string, stripe: string): HTMLCanvasElement {
  let c = tiles.get(t);
  if (c) return c;
  const n = t === 'S2' ? 8 : 6;
  c = document.createElement('canvas');
  c.width = n;
  c.height = n;
  const g = c.getContext('2d')!;
  g.fillStyle = base;
  g.fillRect(0, 0, n, n);
  g.strokeStyle = stripe;
  g.lineWidth = n / 2 / Math.SQRT2;
  g.beginPath();
  for (const o of [-n, 0, n]) {
    g.moveTo(o, n);
    g.lineTo(o + n, 0);
  }
  g.stroke();
  tiles.set(t, c);
  return c;
}
function tierStyle(t: Tier) {
  const p = TIER_PATTERN[t];
  return p ? { color: { image: patternTile(t, TIER_SOLID[t], p), repeat: 'repeat' } } : { color: TIER_SOLID[t] };
}

function box(root: HTMLElement, w: number, h: number): HTMLDivElement {
  const outer = document.createElement('div');
  outer.style.cssText = `background:var(--c-surface);border-radius:22px;padding:16px;width:${w + 32}px`;
  const el = document.createElement('div');
  el.style.cssText = `width:${w}px;height:${h}px`;
  outer.append(el);
  root.append(outer);
  return el;
}

interface Built {
  chart: EChartsType;
  option(p: Palette): EChartsCoreOption;
}

function mount(
  renderer: 'canvas' | 'svg',
  el: HTMLElement,
  reduced: boolean,
  option: (p: Palette) => EChartsCoreOption,
): Built & { finished: Promise<void> } {
  const chart = echarts.init(el, null, { renderer });
  const finished = new Promise<void>((resolve) => chart.on('finished', () => resolve()));
  chart.setOption({ animation: !reduced, ...option(C) });
  return { chart, option, finished };
}

function handleFor(built: (Built & { finished: Promise<void> })[], hover: Element): Handle {
  return {
    hover,
    settled: Promise.all(built.map((b) => b.finished)).then(() => undefined),
    async retheme() {
      const p = { ...C, ...DARK };
      for (const b of built) b.chart.setOption(b.option(p));
      await nextFrame();
    },
  };
}

const base = (p: Palette) => ({
  textStyle: { fontFamily: p.fontSans, color: p.text2 },
  backgroundColor: 'transparent',
});

function forecastOption(p: Palette): EChartsCoreOption {
  const cleanupDays = new Set(FORECAST.cleanups);
  return {
    ...base(p),
    aria: {
      enabled: true,
      label: {
        description:
          'Free space fell from 96 GB to 41 GB over 30 days, with two cleanups adding 22 GB and 9 GB. Forecast reaches 10 GB in about 9 days.',
      },
    },
    grid: { left: 0, right: 0, top: 10, bottom: 20 },
    tooltip: { trigger: 'axis', transitionDuration: 0, axisPointer: { type: 'line', lineStyle: { color: p.hint } } },
    xAxis: { type: 'value', min: 0, max: 40, show: false },
    yAxis: { type: 'value', min: 0, max: 110, splitNumber: 4, axisLine: { show: false }, axisLabel: { show: false }, splitLine: { lineStyle: { color: p.line } } },
    series: [
      {
        name: 'Measured',
        type: 'line',
        symbol: 'none',
        itemStyle: { color: p.ink },
        lineStyle: { color: p.ink, width: 2.5 },
        data: FORECAST.measured.map((d) =>
          cleanupDays.has(d.day)
            ? { value: [d.day, d.gb], symbol: 'rect', symbolSize: 12, itemStyle: { color: p.violet } }
            : [d.day, d.gb],
        ),
        markLine: {
          silent: true,
          symbol: 'none',
          label: { show: false },
          lineStyle: { color: p.hint, type: [4, 4] },
          data: [{ yAxis: FORECAST.floorGb }],
        },
      },
      { name: 'Band low', type: 'line', stack: 'band', symbol: 'none', lineStyle: { opacity: 0 }, data: FORECAST.forecast.map((d) => [d.day, d.lo]), tooltip: { show: false } },
      {
        name: 'Confidence',
        type: 'line',
        stack: 'band',
        symbol: 'none',
        itemStyle: { color: p.violet },
        lineStyle: { opacity: 0 },
        areaStyle: { color: p.violet, opacity: 0.14 },
        data: FORECAST.forecast.map((d) => [d.day, d.hi - d.lo]),
      },
      {
        name: 'Forecast',
        type: 'line',
        symbol: 'none',
        itemStyle: { color: p.violet },
        lineStyle: { color: p.violet, width: 2.5, type: [8, 6] },
        data: FORECAST.forecast.map((d) => [d.day, d.gb]),
      },
    ],
  };
}

function regrowthOption(p: Palette): EChartsCoreOption {
  return {
    ...base(p),
    aria: { enabled: true },
    grid: { left: 0, right: 0, top: 0, bottom: 0 },
    xAxis: { type: 'category', show: false, data: REGROWTH.map((d) => d.label) },
    yAxis: { type: 'value', show: false, max: 1.2 },
    tooltip: { trigger: 'item', transitionDuration: 0 },
    series: [
      {
        type: 'bar',
        barCategoryGap: '8%',
        itemStyle: { borderRadius: 6 },
        data: REGROWTH.map((d) => ({ value: Math.max(0.06, d.gb), itemStyle: { color: d.state === 'installed' ? p.ink : p.plumLight } })),
      },
    ],
  };
}

function duplicationOption(p: Palette): EChartsCoreOption {
  return {
    ...base(p),
    aria: { enabled: true },
    grid: { left: 110, right: 0, top: 0, bottom: 0 },
    xAxis: { type: 'value', show: false, max: 17 },
    yAxis: {
      type: 'category',
      inverse: true,
      data: DUPLICATION.map((d) => d.pkg),
      axisLine: { show: false },
      axisTick: { show: false },
      axisLabel: { color: p.text2, fontFamily: p.fontMono },
    },
    tooltip: { trigger: 'item', transitionDuration: 0 },
    series: [{ type: 'bar', barWidth: '40%', itemStyle: { color: p.ink, borderRadius: 4 }, data: DUPLICATION.map((d) => d.copies) }],
  };
}

function ecosystemOption(p: Palette): EChartsCoreOption {
  return {
    ...base(p),
    aria: { enabled: true },
    grid: { left: 0, right: 0, top: 0, bottom: 18 },
    xAxis: {
      type: 'category',
      data: ECOSYSTEM_MIX.map((m) => m.month),
      axisLine: { show: false },
      axisTick: { show: false },
      axisLabel: { color: p.text2 },
    },
    yAxis: { type: 'value', show: false },
    tooltip: { trigger: 'axis', transitionDuration: 0 },
    series: [...ECOSYSTEMS].reverse().map((k) => ({
      name: ECOSYSTEM_LABEL[k],
      type: 'bar',
      stack: 'mix',
      barCategoryGap: '12%',
      itemStyle: {
        color: k === 'js' ? p.ink : ECO_COLOR[k],
        borderColor: p.surface,
        borderWidth: 1,
        ...(k === 'ml' ? { decal: decal(p.amberLight, 4) } : {}),
      },
      data: ECOSYSTEM_MIX.map((m) => m[k]),
    })),
  };
}

type Node = { name: string; value?: number; children?: Node[]; itemStyle?: object };

function hierarchyData(tree: Tree, p: Palette): Node[] {
  return tree.children.map((c) => ({
    name: c.name,
    itemStyle: { color: p.categoryRing },
    children: c.children.map((l: Leaf) => ({ name: `${l.name}: ${TIER_LABEL[l.tier]}`, value: l.value, itemStyle: tierStyle(l.tier) })),
  }));
}

export function makeRenderer(renderer: 'canvas' | 'svg'): Renderer {
  return {
    name: `echarts-${renderer}`,
    insights(root, reduced) {
      root.style.cssText = 'display:flex;flex-wrap:wrap;gap:12px;align-items:flex-start';
      const f = mount(renderer, box(root, 800, 300), reduced, forecastOption);
      const built = [
        f,
        mount(renderer, box(root, 300, 60), reduced, regrowthOption),
        mount(renderer, box(root, 300, 96), reduced, duplicationOption),
        mount(renderer, box(root, 520, 200), reduced, ecosystemOption),
      ];
      return handleFor(built, f.chart.getDom());
    },
    wheel(root, tree, reduced) {
      const el = box(root, 620, 620);
      const b = mount(renderer, el, reduced, (p) => ({
        ...base(p),
        aria: { enabled: true, label: { description: 'Storage wheel: categories and items by safety tier' } },
        tooltip: { trigger: 'item', transitionDuration: 0 },
        series: [
          {
            type: 'sunburst',
            nodeClick: false,
            sort: 'desc',
            emphasis: { focus: 'none' },
            itemStyle: { borderWidth: 0 },
            levels: [
              {},
              { r0: 122, r: 180, label: { rotate: 'tangential', color: p.ink } },
              { r0: 186, r: 268, label: { show: false } },
            ],
            data: hierarchyData(tree, p),
          },
        ],
      }));
      return handleFor([b], el);
    },
    treemap(root, tree, reduced) {
      const el = box(root, 960, 600);
      const b = mount(renderer, el, reduced, (p) => ({
        ...base(p),
        aria: { enabled: true, label: { description: 'Treemap: categories and items by safety tier' } },
        tooltip: { trigger: 'item', transitionDuration: 0 },
        series: [
          {
            type: 'treemap',
            width: '100%',
            height: '100%',
            roam: false,
            nodeClick: false,
            breadcrumb: { show: false },
            visibleMin: 0,
            label: { show: false },
            upperLabel: { show: false },
            levels: [{ itemStyle: { gapWidth: 2 } }, { itemStyle: { gapWidth: 1 } }],
            data: hierarchyData(tree, p),
          },
        ],
      }));
      return handleFor([b], el);
    },
  };
}
