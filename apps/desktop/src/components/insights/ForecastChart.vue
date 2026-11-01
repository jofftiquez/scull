<script setup lang="ts">
// Free space · history and forecast (design 05). D3 computes the geometry (d3-scale, d3-shape;
// D-37) and Vue renders the SVG, so marks carry ARIA and the colors come from the theme.
// Keyboard: the chart is one tab stop; Left/Right move between days and read the value out.
// A visually hidden table carries the same numbers for screen readers (PRD §14.4).
import { computed, onBeforeUnmount, onMounted, ref } from 'vue';
import { scaleLinear } from 'd3-scale';
import { area, line } from 'd3-shape';
import { formatBytes } from '../../lib/format';
import { FULL_RESERVE, shortDate, type FreeSpaceModel } from './model';

const props = defineProps<{ model: FreeSpaceModel }>();

const H = 300;
const width = ref(800);
const root = ref<HTMLElement | null>(null);
let ro: ResizeObserver | null = null;
onMounted(() => {
  if (!root.value || typeof ResizeObserver === 'undefined') return;
  ro = new ResizeObserver(([e]) => {
    if (e && e.contentRect.width > 0) width.value = Math.round(e.contentRect.width);
  });
  ro.observe(root.value);
});
onBeforeUnmount(() => ro?.disconnect());

const pts = computed(() => props.model.points);
const fc = computed(() => props.model.forecast);
const x = computed(() => {
  const first = pts.value[0]?.at ?? 0;
  const last = fc.value.at(-1)?.at ?? pts.value.at(-1)?.at ?? first + 1;
  return scaleLinear().domain([first, Math.max(last, first + 1)]).range([6, width.value - 6]);
});
const y = computed(() => {
  const max = Math.max(FULL_RESERVE * 2, ...pts.value.map((p) => p.free), ...fc.value.map((p) => p.hi));
  return scaleLinear().domain([0, max * 1.08]).range([H - 12, 14]);
});
const grid = computed(() => y.value.ticks(4).filter((t) => t > 0).map((t) => y.value(t)));
const floorY = computed(() => y.value(FULL_RESERVE));
const measuredPath = computed(
  () =>
    line<{ at: number; free: number }>()
      .x((p) => x.value(p.at))
      .y((p) => y.value(p.free))(pts.value) ?? '',
);
const forecastPath = computed(
  () =>
    line<{ at: number; free: number }>()
      .x((p) => x.value(p.at))
      .y((p) => y.value(p.free))(fc.value) ?? '',
);
const bandPath = computed(
  () =>
    area<{ at: number; lo: number; hi: number }>()
      .x((p) => x.value(p.at))
      .y0((p) => y.value(p.lo))
      .y1((p) => y.value(p.hi))(fc.value) ?? '',
);

/** Every day on the chart, measured then forecast, for keyboard and hover. */
const stops = computed(() => [
  ...pts.value.map((p) => ({ ...p, forecast: false })),
  ...fc.value.slice(1).map((p) => ({ at: p.at, free: p.free, forecast: true })),
]);
const focus = ref<number | null>(null);
const focused = computed(() => (focus.value === null ? null : (stops.value[focus.value] ?? null)));
const readout = computed(() =>
  focused.value
    ? `${shortDate(focused.value.at)}: ${formatBytes(focused.value.free)} free${focused.value.forecast ? ' (forecast)' : ''}`
    : '',
);

function move(delta: number) {
  const n = stops.value.length;
  if (n === 0) return;
  const cur = focus.value ?? pts.value.length - 1;
  focus.value = Math.max(0, Math.min(n - 1, cur + delta));
}
function onKey(e: KeyboardEvent) {
  const step: Record<string, number> = { ArrowRight: 1, ArrowLeft: -1, Home: -Infinity, End: Infinity };
  const d = step[e.key];
  if (d === undefined) return;
  e.preventDefault();
  if (d === -Infinity) focus.value = 0;
  else if (d === Infinity) focus.value = stops.value.length - 1;
  else move(d);
}
function onPointer(e: PointerEvent) {
  const r = (e.currentTarget as Element).getBoundingClientRect();
  if (r.width === 0) return;
  const at = x.value.invert(((e.clientX - r.left) / r.width) * width.value);
  let best = 0;
  stops.value.forEach((s, i) => {
    if (Math.abs(s.at - at) < Math.abs(stops.value[best]!.at - at)) best = i;
  });
  focus.value = best;
}

const today = computed(() => pts.value.at(-1) ?? null);
const label = computed(() => {
  const first = pts.value[0];
  const last = today.value;
  if (!first || !last) return 'No free-space samples recorded yet.';
  const parts = [`Free space went from ${formatBytes(first.free)} to ${formatBytes(last.free)} between ${shortDate(first.at)} and ${shortDate(last.at)}`];
  if (props.model.cleanups.length) {
    parts.push(`with ${props.model.cleanups.length} cleanup${props.model.cleanups.length === 1 ? '' : 's'} adding ${props.model.cleanups.map((c) => formatBytes(c.gained)).join(' and ')}`);
  }
  const t = props.model.trend;
  return `${parts.join(', ')}.${t ? ` Forecast reaches ${formatBytes(FULL_RESERVE)} in about ${t.days} day${t.days === 1 ? '' : 's'}.` : ''}`;
});

/** The captions under the chart: start, each cleanup, today, the full line. */
const captions = computed(() => {
  const out: string[] = [];
  const first = pts.value[0];
  if (first) out.push(`${shortDate(first.at)} · ${formatBytes(first.free)}`);
  for (const c of props.model.cleanups.slice(-2)) out.push(`${shortDate(c.at)} cleanup +${formatBytes(c.gained)}`);
  if (today.value) out.push(`Today · ${formatBytes(today.value.free)}`);
  const end = fc.value.at(-1);
  if (end && props.model.trend) out.push(`${shortDate(end.at)} · ${formatBytes(FULL_RESERVE)} line`);
  return out;
});
</script>

<template>
  <div ref="root" class="relative flex flex-col gap-4">
    <svg
      :viewBox="`0 0 ${width} ${H}`"
      :width="width"
      :height="H"
      class="block h-[300px] w-full touch-none rounded-small outline-offset-4"
      role="img"
      :aria-label="label"
      tabindex="0"
      data-testid="forecast-chart"
      @keydown="onKey"
      @pointermove="onPointer"
      @pointerleave="focus = null"
      @blur="focus = null"
    >
      <line v-for="gy in grid" :key="gy" x1="0" :x2="width" :y1="gy" :y2="gy" stroke="#EEEAE4" />
      <line x1="0" :x2="width" :y1="floorY" :y2="floorY" stroke="var(--color-text-hint)" stroke-dasharray="4 4" />
      <path v-if="fc.length" :d="bandPath" fill="#7C4DFF" fill-opacity="0.12" data-testid="forecast-band" />
      <path :d="measuredPath" fill="none" stroke="var(--color-ink)" stroke-width="2.5" stroke-linejoin="round" />
      <path v-if="fc.length" :d="forecastPath" fill="none" stroke="#7C4DFF" stroke-width="2.5" stroke-dasharray="8 6" data-testid="forecast-line" />
      <rect
        v-for="c in model.cleanups"
        :key="c.at"
        :x="x(c.at) - 6"
        :y="y(c.free) - 6"
        width="12"
        height="12"
        fill="#7C4DFF"
        data-testid="cleanup-marker"
      />
      <rect v-if="today" :x="x(today.at) - 6" :y="y(today.free) - 6" width="12" height="12" fill="var(--color-ink)" />
      <g v-if="focused" aria-hidden="true">
        <line :x1="x(focused.at)" :x2="x(focused.at)" y1="0" :y2="H" stroke="var(--color-text-hint)" />
        <circle :cx="x(focused.at)" :cy="y(focused.free)" r="5" fill="var(--color-ink)" stroke="#FFFFFF" stroke-width="2" />
      </g>
    </svg>
    <div
      v-if="focused"
      class="pointer-events-none absolute rounded-small bg-ink px-2 py-1 font-mono text-caption whitespace-nowrap text-on-ink"
      :style="{ left: `${Math.min(x(focused.at) + 12, width - 220)}px`, top: `${Math.max(0, y(focused.free) - 36)}px` }"
      aria-hidden="true"
      data-testid="forecast-tooltip"
    >
      {{ readout }}
    </div>
    <span class="sr-only" aria-live="polite">{{ readout }}</span>
    <div class="flex flex-wrap justify-between gap-x-3 gap-y-1 font-mono text-[11px] whitespace-nowrap text-text-secondary" aria-hidden="true">
      <span v-for="c in captions" :key="c">{{ c }}</span>
    </div>
    <table class="sr-only">
      <caption>Free space by day</caption>
      <thead>
        <tr><th scope="col">Date</th><th scope="col">Free</th><th scope="col">Kind</th></tr>
      </thead>
      <tbody>
        <tr v-for="s in stops" :key="s.at">
          <td>{{ shortDate(s.at) }}</td>
          <td>{{ formatBytes(s.free) }}</td>
          <td>{{ s.forecast ? 'forecast' : model.cleanups.some((c) => c.at === s.at) ? 'measured, after a cleanup' : 'measured' }}</td>
        </tr>
      </tbody>
    </table>
  </div>
</template>
