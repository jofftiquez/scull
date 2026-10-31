<script setup lang="ts">
// The storage wheel (D-19; design 01): inner ring = categories, outer ring = items colored
// AND patterned by tier (violet solid, lilac solid, amber stripes, plum hatch). While a scan
// runs, a radar arm turns and the wheel is revealed behind it (D-20, design 01b).
//
// Keyboard: the wheel is one tab stop; arrow keys move between segments, Enter/Space selects,
// Shift+Enter adds the focused item to cleanup.
import { computed, nextTick, ref, useId } from 'vue';
import type { Tier } from '../../bridge';
import { formatBytes } from '../../lib/format';
import { TIERS, type TierFilter } from './tiers';
import { GEOMETRY as G, itemOpacity, polar, type CategoryArc } from './wheel';
import type { WheelCenter } from './center';

const props = defineProps<{
  arcs: CategoryArc[];
  filter: TierFilter;
  selected: string | null;
  center: WheelCenter;
  /** Arm position 0..1; the wheel is revealed up to it. */
  sweep: number;
  /** The radar arm and the spinning ring are shown. */
  scanning: boolean;
  /** Bumped after a sweep completes: the center bounces once. */
  bounceKey?: number;
  /** Items freed a moment ago (fade to 0.1). */
  cleaned?: ReadonlySet<string>;
}>();

const emit = defineEmits<{ select: [id: string | null, add: boolean] }>();

const uid = useId();
const ids = { stripes: `${uid}-stripes`, hatch: `${uid}-hatch`, reveal: `${uid}-reveal` };

const FILL: Record<Tier, string> = {
  S0: TIERS.S0.swatch,
  S1: TIERS.S1.swatch,
  S2: `url(#${ids.stripes})`,
  S3: `url(#${ids.hatch})`,
};

interface Seg {
  id: string;
  label: string;
}

/** Every focusable segment in reading order: each category, then its items. */
const segments = computed<Seg[]>(() =>
  props.arcs.flatMap((c) => [
    {
      id: c.category.key,
      label: `${c.category.label}: ${formatBytes(c.category.bytes)}, ${c.category.count} item${c.category.count === 1 ? '' : 's'}, most protective tier ${TIERS[c.category.tier].label}`,
    },
    ...c.items.map((a) => ({
      id: a.item.id,
      label: `${a.item.name}: ${TIERS[a.item.tier].label}, ${formatBytes(a.item.bytes)}`,
    })),
  ]),
);

const focusIndex = ref(0);
const svg = ref<SVGSVGElement | null>(null);
const tabTarget = computed(() => {
  const segs = segments.value;
  if (segs.length === 0) return null;
  const sel = segs.findIndex((s) => s.id === props.selected);
  return segs[sel >= 0 ? sel : Math.min(focusIndex.value, segs.length - 1)]!.id;
});

function onKey(e: KeyboardEvent, id: string) {
  const segs = segments.value;
  const i = segs.findIndex((s) => s.id === id);
  let next: number;
  if (e.key === 'ArrowRight' || e.key === 'ArrowDown') next = (i + 1) % segs.length;
  else if (e.key === 'ArrowLeft' || e.key === 'ArrowUp') next = (i - 1 + segs.length) % segs.length;
  else if (e.key === 'Home') next = 0;
  else if (e.key === 'End') next = segs.length - 1;
  else if (e.key === 'Enter' || e.key === ' ') {
    e.preventDefault();
    emit('select', id, e.shiftKey);
    return;
  } else return;
  e.preventDefault();
  focusIndex.value = next;
  const target = segs[next]!.id;
  void nextTick(() => svg.value?.querySelector<SVGElement>(`[data-seg="${CSS.escape(target)}"]`)?.focus());
}

function onClick(e: MouseEvent, id: string) {
  focusIndex.value = segments.value.findIndex((s) => s.id === id);
  emit('select', id, e.shiftKey);
}

const arm = computed(() => {
  const [x, y] = polar(G.armLength, props.sweep * 360);
  return { x: x.toFixed(1), y: y.toFixed(1) };
});
const reveal = computed(() => `${(props.sweep * G.maskLength).toFixed(1)} ${G.maskLength}`);
const empty = computed(() => props.arcs.length === 0);
</script>

<template>
  <svg
    ref="svg"
    :viewBox="`0 0 ${G.size} ${G.size}`"
    class="wheel h-auto w-full max-w-[660px]"
    role="group"
    aria-label="Storage wheel: inner ring shows categories, outer ring shows items colored by risk tier. Use the arrow keys to move between segments and Enter to inspect one."
    data-testid="wheel"
  >
    <defs>
      <pattern :id="ids.stripes" patternUnits="userSpaceOnUse" width="10" height="10" patternTransform="rotate(45)">
        <rect width="10" height="10" fill="#FFB547" />
        <rect width="5" height="10" fill="#FFD08A" />
      </pattern>
      <pattern :id="ids.hatch" patternUnits="userSpaceOnUse" width="8" height="8" patternTransform="rotate(45)">
        <rect width="8" height="8" fill="#3B2A4F" />
        <rect width="2" height="8" fill="#5A4772" />
      </pattern>
      <mask :id="ids.reveal" maskUnits="userSpaceOnUse" x="0" y="0" :width="G.size" :height="G.size">
        <circle
          :cx="G.c"
          :cy="G.c"
          :r="G.maskRadius"
          fill="none"
          stroke="#FFFFFF"
          :stroke-width="G.maskWidth"
          :stroke-dasharray="reveal"
          :transform="`rotate(-90 ${G.c} ${G.c})`"
          data-testid="reveal"
        />
      </mask>
    </defs>

    <circle
      class="ring-dotted"
      :class="{ spinning: scanning }"
      :cx="G.c"
      :cy="G.c"
      :r="G.dotted"
      fill="none"
      stroke="#D6268A"
      :stroke-opacity="scanning ? 0.9 : 0.45"
      stroke-width="1.5"
      stroke-dasharray="1 7"
      stroke-linecap="round"
      data-testid="ring"
    />
    <circle :cx="G.c" :cy="G.c" :r="G.faint" fill="none" stroke="#7C4DFF" stroke-opacity="0.18" stroke-width="1" />

    <!-- Empty track: the wheel's shape before the first result. -->
    <g v-if="empty" aria-hidden="true">
      <circle :cx="G.c" :cy="G.c" :r="(G.categoryInner + G.categoryOuter) / 2" fill="none" stroke="#F2EFEA" :stroke-width="G.categoryOuter - G.categoryInner" />
      <circle :cx="G.c" :cy="G.c" :r="(G.itemInner + G.itemOuter) / 2" fill="none" stroke="#F7F5F2" :stroke-width="G.itemOuter - G.itemInner" />
    </g>

    <g :mask="`url(#${ids.reveal})`">
      <template v-for="cat in arcs" :key="cat.category.key">
        <path
          :d="cat.d"
          fill="#EEEAF4"
          class="seg"
          :stroke="!scanning && selected === cat.category.key ? '#D6268A' : 'none'"
          stroke-width="3"
          role="button"
          :tabindex="tabTarget === cat.category.key ? 0 : -1"
          :aria-pressed="selected === cat.category.key"
          :aria-label="segments.find((s) => s.id === cat.category.key)?.label"
          :data-seg="cat.category.key"
          data-kind="category"
          @click="onClick($event, cat.category.key)"
          @keydown="onKey($event, cat.category.key)"
        >
          <title>{{ cat.category.label }} · {{ formatBytes(cat.category.bytes) }}</title>
        </path>
        <text
          v-if="cat.label"
          :x="cat.label.x"
          :y="cat.label.y"
          text-anchor="middle"
          dominant-baseline="middle"
          font-size="11"
          font-weight="500"
          fill="#4A4450"
          pointer-events="none"
          aria-hidden="true"
          :transform="`rotate(${cat.label.rotate} ${cat.label.x} ${cat.label.y})`"
        >{{ cat.category.label }}</text>
        <path
          v-for="a in cat.items"
          :key="a.item.id"
          :d="a.d"
          :fill="FILL[a.item.tier]"
          :opacity="itemOpacity(a.item, { filter, selected, cleaned })"
          :stroke="!scanning && selected === a.item.id ? '#D6268A' : 'none'"
          stroke-width="3"
          class="seg item"
          role="button"
          :tabindex="tabTarget === a.item.id ? 0 : -1"
          :aria-pressed="selected === a.item.id"
          :aria-label="segments.find((s) => s.id === a.item.id)?.label"
          :data-seg="a.item.id"
          :data-tier="a.item.tier"
          data-kind="item"
          @click="onClick($event, a.item.id)"
          @keydown="onKey($event, a.item.id)"
        >
          <title>{{ a.item.name }} · {{ formatBytes(a.item.bytes) }}</title>
        </path>
      </template>
    </g>

    <g aria-hidden="true" class="arm" :style="{ opacity: scanning ? 1 : 0 }" data-testid="arm">
      <line :x1="G.c" :y1="G.c" :x2="arm.x" :y2="arm.y" stroke="#D6268A" stroke-width="2.5" stroke-linecap="round" />
      <circle :cx="arm.x" :cy="arm.y" r="6" fill="#D6268A" />
    </g>

    <circle :cx="G.c" :cy="G.c" :r="G.hub" fill="#F7F5F2" />
    <g :key="bounceKey ?? 0" :class="{ bounce: (bounceKey ?? 0) > 0 }" aria-live="polite" data-testid="center">
      <text :x="G.c" y="276" text-anchor="middle" font-size="13" font-weight="600" :fill="center.kColor">{{ center.k }}</text>
      <text :x="G.c" y="322" text-anchor="middle" font-size="40" font-weight="700" fill="#15121A" class="tabular-nums">{{ center.v }}</text>
      <text :x="G.c" y="352" text-anchor="middle" font-size="13" fill="#615B66">{{ center.n }}</text>
    </g>
  </svg>
</template>

<style scoped>
.wheel text {
  font-family: var(--font-sans);
}
.seg {
  cursor: pointer;
  transition: opacity 250ms ease;
  outline: none;
}
.seg:focus-visible {
  stroke: var(--color-ink);
  stroke-width: 3;
  stroke-dasharray: 5 3;
}
.ring-dotted {
  transform-box: view-box;
  transform-origin: 310px 310px;
  transition: stroke-opacity 300ms ease;
}
.ring-dotted.spinning {
  animation: scull-spin 6s linear infinite;
}
.arm {
  transition: opacity 300ms ease;
}
.bounce {
  transform-box: fill-box;
  transform-origin: center;
  animation: scull-bounce 620ms cubic-bezier(0.2, 0.9, 0.3, 1.3);
}
@keyframes scull-bounce {
  0% {
    transform: scale(0.4);
    opacity: 0;
  }
  55% {
    transform: scale(1.12);
    opacity: 1;
  }
  75% {
    transform: scale(0.96);
  }
  100% {
    transform: scale(1);
  }
}
</style>
