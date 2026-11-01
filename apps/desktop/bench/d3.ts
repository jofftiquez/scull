// D3 (ISC) + SVG renderer: d3-scale/d3-shape/d3-hierarchy compute geometry, d3-selection
// writes SVG. Colors are CSS custom properties from tokens.json; tier and series patterns are
// SVG <pattern>s; marks carry ARIA labels; CSS animations are dropped under reduced motion.
import { scaleBand, scaleLinear } from 'd3-scale';
import { area, arc, line } from 'd3-shape';
import { hierarchy, partition, treemap, type HierarchyNode, type HierarchyRectangularNode } from 'd3-hierarchy';
import { select, type Selection } from 'd3-selection';
import {
  DUPLICATION,
  ECOSYSTEMS,
  ECOSYSTEM_LABEL,
  ECOSYSTEM_MIX,
  FORECAST,
  REGROWTH,
  type Leaf,
  type Tier,
  type Tree,
} from './data';
import { DARK, TIER_LABEL } from './theme';
import { nextFrame, runBench, type Handle, type Renderer } from './harness';

const SVG_NS = 'http://www.w3.org/2000/svg';
type Svg = Selection<SVGSVGElement, unknown, null, undefined>;

const STYLE = `
  .d3-chart { font: 11px var(--font, 'DM Sans', system-ui, sans-serif); color: var(--c-ink); }
  .d3-chart text { fill: var(--c-text2); }
  .d3-rise { transform-box: fill-box; transform-origin: bottom; animation: d3-rise 340ms ease-out; }
  @keyframes d3-rise { from { transform: scaleY(0); } to { transform: scaleY(1); } }
  @media (prefers-reduced-motion: reduce) { .d3-rise { animation: none; } }
  .d3-mark:hover, .d3-mark.is-hot { stroke: var(--c-ink); stroke-width: 2; }
  .d3-tip { position: absolute; pointer-events: none; background: var(--c-ink); color: var(--c-surface);
    font: 12px 'DM Mono', ui-monospace, monospace; padding: 4px 8px; border-radius: 8px; white-space: nowrap; }
`;

function ensureStyle(): void {
  if (document.getElementById('d3-style')) return;
  const s = document.createElement('style');
  s.id = 'd3-style';
  s.textContent = STYLE;
  document.head.append(s);
}

/** Tier fill: solid for S0/S1, the stripes/hatch pattern for S2/S3 (color AND pattern). */
const TIER_FILL: Record<Tier, string> = {
  S0: 'var(--c-violet)',
  S1: 'var(--c-lilac)',
  S2: 'url(#p-stripes)',
  S3: 'url(#p-hatch)',
};

function defs(svg: Svg): void {
  const d = svg.append('defs');
  const pat = (id: string, size: number, a: string, b: string) => {
    const p = d
      .append('pattern')
      .attr('id', id)
      .attr('width', size * 2)
      .attr('height', size * 2)
      .attr('patternUnits', 'userSpaceOnUse')
      .attr('patternTransform', 'rotate(45)');
    p.append('rect').attr('width', size * 2).attr('height', size * 2).attr('fill', a);
    p.append('rect').attr('width', size).attr('height', size * 2).attr('fill', b);
  };
  pat('p-stripes', 4, 'var(--c-amber)', 'var(--c-amberLight)');
  pat('p-hatch', 3, 'var(--c-plum)', 'var(--c-plumLight)');
}

function chart(parent: HTMLElement, w: number, h: number, label: string): Svg {
  const svg = select(parent)
    .append('svg')
    .attr('xmlns', SVG_NS)
    .attr('class', 'd3-chart')
    .attr('viewBox', `0 0 ${w} ${h}`)
    .attr('width', w)
    .attr('height', h)
    .attr('role', 'group')
    .attr('aria-label', label);
  defs(svg);
  return svg;
}

function tip(parent: HTMLElement): HTMLDivElement {
  const t = document.createElement('div');
  t.className = 'd3-tip';
  t.hidden = true;
  parent.style.position = 'relative';
  parent.append(t);
  return t;
}

function settledAfterAnimations(reduced: boolean): Promise<void> {
  if (reduced) return Promise.resolve();
  return Promise.all(document.getAnimations().map((a) => a.finished)).then(() => undefined);
}

async function retheme(): Promise<void> {
  for (const [k, v] of Object.entries(DARK)) document.documentElement.style.setProperty(`--c-${k}`, v);
  await nextFrame();
}

function box(root: HTMLElement, w: number): HTMLDivElement {
  const el = document.createElement('div');
  el.style.width = `${w}px`;
  el.style.background = 'var(--c-surface)';
  el.style.borderRadius = '22px';
  el.style.padding = '16px';
  root.append(el);
  return el;
}

// ---- Insights ----

function forecast(root: HTMLElement): Element {
  const W = 800;
  const H = 300;
  const holder = box(root, W + 32);
  const svg = chart(
    holder,
    W,
    H,
    'Free space fell from 96 GB to 41 GB over 30 days, with two cleanups adding 22 GB and 9 GB. Forecast reaches 10 GB in about 9 days.',
  );
  svg.attr('tabindex', 0);
  const x = scaleLinear().domain([0, 40]).range([0, W]);
  const y = scaleLinear().domain([0, 110]).range([H - 20, 10]);
  const g = svg.append('g');
  g.selectAll('line.grid')
    .data(y.ticks(4))
    .join('line')
    .attr('x1', 0)
    .attr('x2', W)
    .attr('y1', (d) => y(d))
    .attr('y2', (d) => y(d))
    .attr('stroke', 'var(--c-line)');
  g.append('line')
    .attr('x1', 0)
    .attr('x2', W)
    .attr('y1', y(FORECAST.floorGb))
    .attr('y2', y(FORECAST.floorGb))
    .attr('stroke', 'var(--c-hint)')
    .attr('stroke-dasharray', '4 4');
  const band = area<{ day: number; lo: number; hi: number }>()
    .x((d) => x(d.day))
    .y0((d) => y(d.lo))
    .y1((d) => y(d.hi));
  g.append('path')
    .attr('d', band(FORECAST.forecast))
    .attr('fill', 'var(--c-violet)')
    .attr('fill-opacity', 0.14)
    .attr('aria-hidden', 'true');
  const ln = line<{ day: number; gb: number }>()
    .x((d) => x(d.day))
    .y((d) => y(d.gb));
  g.append('path').attr('d', ln(FORECAST.measured)).attr('fill', 'none').attr('stroke', 'var(--c-ink)').attr('stroke-width', 2.5);
  g.append('path')
    .attr('d', ln(FORECAST.forecast))
    .attr('fill', 'none')
    .attr('stroke', 'var(--c-violet)')
    .attr('stroke-width', 2.5)
    .attr('stroke-dasharray', '8 6');
  g.append('g')
    .attr('role', 'list')
    .attr('aria-label', 'Cleanups')
    .selectAll('rect')
    .data(FORECAST.cleanups.map((d) => FORECAST.measured[d]!))
    .join('rect')
    .attr('role', 'listitem')
    .attr('aria-label', (d) => `Cleanup on day ${d.day}: ${d.gb} GB free after`)
    .attr('x', (d) => x(d.day) - 6)
    .attr('y', (d) => y(d.gb) - 6)
    .attr('width', 12)
    .attr('height', 12)
    .attr('fill', 'var(--c-violet)');

  // Hover / keyboard: a rule and a tooltip at the nearest day.
  const all = [...FORECAST.measured, ...FORECAST.forecast.slice(1)];
  const rule = g.append('line').attr('y1', 0).attr('y2', H).attr('stroke', 'var(--c-hint)').attr('visibility', 'hidden');
  const dot = g.append('circle').attr('r', 5).attr('fill', 'var(--c-ink)').attr('visibility', 'hidden');
  const t = tip(holder);
  const live = document.createElement('span');
  live.setAttribute('aria-live', 'polite');
  live.style.cssText = 'position:absolute;width:1px;height:1px;overflow:hidden;clip-path:inset(50%)';
  holder.append(live);
  let at = all.length - 11;
  const show = (i: number) => {
    at = Math.max(0, Math.min(all.length - 1, i));
    const p = all[at]!;
    rule.attr('x1', x(p.day)).attr('x2', x(p.day)).attr('visibility', 'visible');
    dot.attr('cx', x(p.day)).attr('cy', y(p.gb)).attr('visibility', 'visible');
    t.hidden = false;
    t.textContent = `Day ${p.day}: ${p.gb.toFixed(1)} GB${p.day > 30 ? ' (forecast)' : ''}`;
    t.style.left = `${x(p.day) + 24}px`;
    t.style.top = `${y(p.gb) - 12}px`;
    live.textContent = t.textContent;
  };
  const node = svg.node()!;
  node.addEventListener('pointermove', (e) => {
    const r = node.getBoundingClientRect();
    show(Math.round(x.invert(((e.clientX - r.left) / r.width) * W)));
  });
  node.addEventListener('keydown', (e) => {
    if (e.key === 'ArrowRight') show(at + 1);
    else if (e.key === 'ArrowLeft') show(at - 1);
  });
  return node;
}

function regrowth(root: HTMLElement): void {
  const W = 300;
  const H = 60;
  const svg = chart(box(root, W + 32), W, H, 'shop-api/node_modules came back 4 times this month');
  const x = scaleBand<number>()
    .domain(REGROWTH.map((_, i) => i))
    .range([0, W])
    .paddingInner(0.08);
  const y = scaleLinear().domain([0, 1.2]).range([0, H]);
  svg
    .append('g')
    .attr('role', 'list')
    .selectAll('rect')
    .data(REGROWTH)
    .join('rect')
    .attr('class', 'd3-rise d3-mark')
    .attr('role', 'listitem')
    .attr('aria-label', (d) => `${d.label}: ${d.state}, ${d.gb} GB`)
    .attr('x', (_, i) => x(i)!)
    .attr('width', x.bandwidth())
    .attr('y', (d) => H - Math.max(3, y(d.gb)))
    .attr('height', (d) => Math.max(3, y(d.gb)))
    .attr('rx', 6)
    .attr('fill', (d) => (d.state === 'installed' ? 'var(--c-ink)' : 'var(--c-plumLight)'));
}

function duplication(root: HTMLElement): void {
  const W = 300;
  const H = 96;
  const svg = chart(box(root, W + 32), W, H, '7.4 GB of identical packages');
  const y = scaleBand<string>()
    .domain(DUPLICATION.map((d) => d.pkg))
    .range([0, H])
    .padding(0.3);
  const x = scaleLinear().domain([0, 17]).range([0, W - 110]);
  const g = svg.append('g').attr('role', 'list');
  const row = g
    .selectAll('g')
    .data(DUPLICATION)
    .join('g')
    .attr('role', 'listitem')
    .attr('aria-label', (d) => `${d.pkg} in ${d.copies} projects, ${d.gb} GB`);
  row
    .append('text')
    .attr('x', 0)
    .attr('y', (d) => y(d.pkg)! + y.bandwidth() / 2 + 4)
    .attr('aria-hidden', 'true')
    .text((d) => d.pkg);
  row
    .append('rect')
    .attr('class', 'd3-mark')
    .attr('x', 110)
    .attr('y', (d) => y(d.pkg)!)
    .attr('height', y.bandwidth())
    .attr('width', (d) => x(d.copies))
    .attr('rx', 4)
    .attr('fill', 'var(--c-ink)');
}

function ecosystem(root: HTMLElement): void {
  const W = 520;
  const H = 200;
  const svg = chart(box(root, W + 32), W, H, 'Development storage by ecosystem per month, May to October');
  const x = scaleBand<string>()
    .domain(ECOSYSTEM_MIX.map((m) => m.month))
    .range([0, W])
    .paddingInner(0.12);
  const max = Math.max(...ECOSYSTEM_MIX.map((m) => ECOSYSTEMS.reduce((a, k) => a + m[k], 0)));
  const y = scaleLinear().domain([0, max]).range([0, H - 18]);
  const fill: Record<string, string> = {
    ml: 'url(#p-stripes)',
    js: 'var(--c-ink)',
    rust: 'var(--c-violet)',
    other: 'var(--c-plumLight)',
  };
  const cols = svg
    .append('g')
    .attr('role', 'list')
    .selectAll('g')
    .data(ECOSYSTEM_MIX)
    .join('g')
    .attr('role', 'listitem')
    .attr('class', 'd3-rise')
    .attr('aria-label', (m) => `${m.month}: ${ECOSYSTEMS.map((k) => `${ECOSYSTEM_LABEL[k]} ${m[k].toFixed(0)} GB`).join(', ')}`);
  cols.each(function (m) {
    let top = H - 18;
    for (const k of [...ECOSYSTEMS].reverse()) {
      const h = y(m[k]);
      top -= h;
      select(this)
        .append('rect')
        .attr('class', 'd3-mark')
        .attr('x', x(m.month)!)
        .attr('width', x.bandwidth())
        .attr('y', top + 1)
        .attr('height', Math.max(0, h - 2))
        .attr('fill', fill[k]!);
    }
  });
  svg
    .append('g')
    .attr('aria-hidden', 'true')
    .selectAll('text')
    .data(ECOSYSTEM_MIX)
    .join('text')
    .attr('x', (m) => x(m.month)!)
    .attr('y', H - 2)
    .text((m) => m.month);
}

// ---- Hierarchy ----

type Node = Tree | Tree['children'][number] | Leaf;
const value = (d: Node) => ('value' in d ? d.value : 0);

function layoutRoot(tree: Tree): HierarchyNode<Node> {
  return hierarchy<Node>(tree, (d) => ('children' in d ? (d.children as Node[]) : undefined))
    .sum(value)
    .sort((a, b) => (b.value ?? 0) - (a.value ?? 0));
}

function hoverDelegation(svg: SVGSVGElement, holder: HTMLElement, labelOf: (el: Element) => string | null): void {
  const t = tip(holder);
  let hot: Element | null = null;
  svg.addEventListener('pointermove', (e) => {
    const el = e.target as Element;
    if (el === hot) return;
    hot?.classList.remove('is-hot');
    const label = labelOf(el);
    if (!label) {
      hot = null;
      t.hidden = true;
      return;
    }
    hot = el;
    el.classList.add('is-hot');
    t.hidden = false;
    t.textContent = label;
    t.style.left = `${e.offsetX + 16}px`;
    t.style.top = `${e.offsetY + 16}px`;
  });
}

function wheel(root: HTMLElement, tree: Tree, reduced: boolean): Handle {
  ensureStyle();
  const S = 620;
  const holder = box(root, S + 32);
  const svg = chart(holder, S, S, 'Storage wheel: categories and items by safety tier');
  const r = partition<Node>().size([2 * Math.PI, 3])(layoutRoot(tree));
  const radius: Record<number, [number, number]> = { 1: [122, 180], 2: [186, 268] };
  const a = arc<HierarchyRectangularNode<Node>>()
    .startAngle((d) => d.x0)
    .endAngle((d) => d.x1)
    .innerRadius((d) => radius[d.depth]![0])
    .outerRadius((d) => radius[d.depth]![1])
    .padAngle(0.002);
  const g = svg.append('g').attr('transform', `translate(${S / 2},${S / 2})`).attr('role', 'list');
  const nodes = r.descendants().filter((d) => d.depth > 0);
  g.selectAll('path')
    .data(nodes)
    .join('path')
    .attr('class', 'd3-mark')
    .attr('d', (d) => a(d))
    .attr('fill', (d) => (d.depth === 1 ? 'var(--c-categoryRing)' : TIER_FILL[(d.data as Leaf).tier]))
    .attr('role', 'listitem')
    .attr('aria-label', (d) =>
      d.depth === 1
        ? `${d.data.name}: ${d.children?.length ?? 0} items`
        : `${d.data.name}: ${TIER_LABEL[(d.data as Leaf).tier]}`,
    );
  const labels = arc<HierarchyRectangularNode<Node>>()
    .innerRadius(151)
    .outerRadius(151)
    .startAngle((d) => d.x0)
    .endAngle((d) => d.x1);
  g.append('g')
    .attr('aria-hidden', 'true')
    .selectAll('text')
    .data(r.children ?? [])
    .join('text')
    .attr('transform', (d) => `translate(${labels.centroid(d)})`)
    .attr('text-anchor', 'middle')
    .attr('pointer-events', 'none')
    .style('fill', 'var(--c-ink)')
    .text((d) => d.data.name);
  const node = svg.node()!;
  hoverDelegation(node, holder, (el) => el.getAttribute('aria-label'));
  return { hover: node, settled: settledAfterAnimations(reduced), retheme };
}

function tree(root: HTMLElement, data: Tree, reduced: boolean): Handle {
  ensureStyle();
  const W = 960;
  const H = 600;
  const holder = box(root, W + 32);
  const svg = chart(holder, W, H, 'Treemap: categories and items by safety tier');
  const r = treemap<Node>().size([W, H]).paddingInner(1).paddingOuter(2)(layoutRoot(data));
  const g = svg.append('g').attr('role', 'list');
  g.selectAll('rect')
    .data(r.leaves())
    .join('rect')
    .attr('class', 'd3-mark')
    .attr('x', (d) => d.x0)
    .attr('y', (d) => d.y0)
    .attr('width', (d) => Math.max(0, d.x1 - d.x0))
    .attr('height', (d) => Math.max(0, d.y1 - d.y0))
    .attr('fill', (d) => TIER_FILL[(d.data as Leaf).tier])
    .attr('role', 'listitem')
    .attr('aria-label', (d) => `${d.data.name}: ${TIER_LABEL[(d.data as Leaf).tier]}`);
  const node = svg.node()!;
  hoverDelegation(node, holder, (el) => el.getAttribute('aria-label'));
  return { hover: node, settled: settledAfterAnimations(reduced), retheme };
}

export const d3Renderer: Renderer = {
  name: 'd3-svg',
  insights(root, reduced) {
    ensureStyle();
    root.style.cssText = 'display:flex;flex-wrap:wrap;gap:12px;align-items:flex-start';
    const hover = forecast(root);
    regrowth(root);
    duplication(root);
    ecosystem(root);
    return { hover, settled: settledAfterAnimations(reduced), retheme };
  },
  wheel,
  treemap: tree,
};

void runBench(d3Renderer);
