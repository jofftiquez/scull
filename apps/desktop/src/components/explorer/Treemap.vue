<script setup lang="ts">
// The Explorer's treemap (design 07 / 07b), drawn as SVG from the squarified layout in
// treemap.ts. Each tile is a real button (role, focus, Enter/Space): folders open, items and
// groups are inspected. Tiers are never told by color alone: uncertain tiles are striped,
// protected ones hatched, and every tile's accessible name carries its tier.
import { computed, onBeforeUnmount, onMounted, ref } from 'vue';
import { formatBytes } from '../../lib/format';
import { TIERS } from '../overview/tiers';
import { driveKindLabel, opens, type ExNode } from './model';
import { layoutTreemap } from './treemap';

const props = withDefaults(
  defineProps<{
    nodes: ExNode[];
    /** Total of the folder shown (tile font sizes are relative to it). */
    total: number;
    selected?: string | null;
    height?: number;
  }>(),
  { selected: null, height: 560 },
);
const emit = defineEmits<{ open: [node: ExNode]; select: [node: ExNode] }>();

/** Gap between tiles and corner radius (design: 6px, 16px). */
const GAP = 6;
const RADIUS = 16;
const PAD = 14;

const uid = `tm-${Math.random().toString(36).slice(2, 9)}`;

const box = ref<HTMLElement | null>(null);
const width = ref(734);
let observer: ResizeObserver | null = null;

onMounted(() => {
  const el = box.value;
  if (!el) return;
  if (el.clientWidth > 0) width.value = el.clientWidth;
  if (typeof ResizeObserver !== 'undefined') {
    observer = new ResizeObserver((entries) => {
      const w = entries[0]?.contentRect.width;
      if (w && w > 0) width.value = w;
    });
    observer.observe(el);
  }
});
onBeforeUnmount(() => observer?.disconnect());

interface Look {
  fill: string;
  fg: string;
}

function look(n: ExNode): Look {
  if (n.kind === 'drive') return { fill: 'var(--color-drive-tile)', fg: 'var(--color-on-drive-tile)' };
  if (n.kind === 'folder') return { fill: 'var(--color-category-ring)', fg: 'var(--color-ink)' };
  if (n.kind === 'group') return { fill: 'var(--color-surface-raised)', fg: 'var(--color-ink)' };
  switch (n.tier) {
    case 'S0':
      return { fill: 'var(--color-tier-s0)', fg: 'var(--color-tier-s0-text)' };
    case 'S1':
      return { fill: 'var(--color-tier-s1)', fg: 'var(--color-tier-s1-text)' };
    case 'S2':
      return { fill: `url(#${uid}-s2)`, fg: 'var(--color-tier-s2-text)' };
    default:
      return { fill: `url(#${uid}-s3)`, fg: 'var(--color-tier-s3-text)' };
  }
}

/** Fit text to a width (approximate glyph width), with an ellipsis. */
function fit(text: string, px: number, glyph: number): string {
  const max = Math.floor(px / glyph);
  if (max <= 1) return '';
  return text.length <= max ? text : `${text.slice(0, max - 1)}…`;
}

function title(n: ExNode): string {
  return n.kind === 'drive' && n.drive ? `${n.name} · ${driveKindLabel(n.drive)}` : n.name;
}

function aria(n: ExNode): string {
  const size = formatBytes(n.bytes);
  if (opens(n)) return `Open ${n.kind === 'drive' ? 'drive' : 'folder'} ${title(n)}, ${size}`;
  if (n.kind === 'group') return `Inspect ${n.name}, ${size}`;
  return `Inspect ${n.name}, ${size}, ${TIERS[n.tier!].label.toLowerCase()}`;
}

const tiles = computed(() =>
  layoutTreemap(props.nodes, (n) => n.bytes, width.value, props.height, GAP).map((t) => {
    const n = t.item;
    const lk = look(n);
    const big = props.total > 0 && n.bytes / props.total > 0.15;
    const more = opens(n);
    const room = t.w - PAD * 2;
    const showName = t.w >= 44 && t.h >= 40;
    const showSize = showName && t.h >= 72;
    const fs = big ? 20 : 13;
    return {
      ...t,
      node: n,
      ...lk,
      more,
      label: showName ? fit(title(n), room - (more ? 12 : 0), 7.9) : '',
      size: showSize ? fit(formatBytes(n.bytes), room, fs * 0.62) : '',
      fs,
      aria: aria(n),
    };
  }),
);

function activate(n: ExNode) {
  if (opens(n)) emit('open', n);
  else emit('select', n);
}
</script>

<template>
  <div ref="box" class="relative w-full" :style="{ height: `${height}px` }" data-testid="treemap">
    <svg
      :width="width"
      :height="height"
      :viewBox="`0 0 ${width} ${height}`"
      class="block overflow-visible"
      role="group"
      aria-label="Folders by size"
    >
      <defs>
        <pattern :id="`${uid}-s2`" patternUnits="userSpaceOnUse" width="8" height="8" patternTransform="rotate(45)">
          <rect width="4" height="8" fill="var(--color-tier-s2)" />
          <rect x="4" width="4" height="8" fill="var(--color-tier-s2-stripe)" />
        </pattern>
        <pattern :id="`${uid}-s3`" patternUnits="userSpaceOnUse" width="6" height="6" patternTransform="rotate(45)">
          <rect width="3" height="6" fill="var(--color-tier-s3)" />
          <rect x="3" width="3" height="6" fill="var(--color-tier-s3-stripe)" />
        </pattern>
        <clipPath v-for="(t, i) in tiles" :id="`${uid}-c${i}`" :key="t.node.id">
          <rect :x="t.x" :y="t.y" :width="t.w" :height="t.h" :rx="RADIUS" />
        </clipPath>
      </defs>
      <g
        v-for="(t, i) in tiles"
        :key="t.node.id"
        class="tile"
        role="button"
        tabindex="0"
        :aria-label="t.aria"
        :aria-pressed="t.more ? undefined : selected === t.node.id"
        :data-kind="t.node.kind"
        :data-id="t.node.id"
        data-testid="tile"
        @click="activate(t.node)"
        @keydown.enter.prevent="activate(t.node)"
        @keydown.space.prevent="activate(t.node)"
      >
        <rect class="bg" :x="t.x" :y="t.y" :width="t.w" :height="t.h" :rx="RADIUS" :fill="t.fill" />
        <g :clip-path="`url(#${uid}-c${i})`" :fill="t.fg" aria-hidden="true">
          <text v-if="t.label" :x="t.x + PAD" :y="t.y + PAD + 15" class="name">
            {{ t.label }}<tspan v-if="t.more" dx="6" class="more">›</tspan>
          </text>
          <text v-if="t.size" :x="t.x + PAD" :y="t.y + t.h - (t.fs === 20 ? 22 : 18)" class="size" :font-size="t.fs">{{ t.size }}</text>
        </g>
        <rect
          v-if="selected === t.node.id"
          class="ring"
          :x="t.x + 1.5"
          :y="t.y + 1.5"
          :width="Math.max(0, t.w - 3)"
          :height="Math.max(0, t.h - 3)"
          :rx="RADIUS - 1.5"
          data-testid="selected-ring"
        />
        <rect class="focus" :x="t.x - 3" :y="t.y - 3" :width="t.w + 6" :height="t.h + 6" :rx="RADIUS + 3" />
      </g>
    </svg>
  </div>
</template>

<style scoped>
.tile {
  cursor: pointer;
  outline: none;
}
.tile .bg {
  transition: opacity 200ms ease;
}
.tile:hover .bg {
  opacity: 0.9;
}
.name {
  font-family: var(--font-sans);
  font-size: 14px;
  font-weight: 600;
}
.more {
  opacity: 0.7;
  font-weight: 400;
}
.size {
  font-family: var(--font-mono);
  font-weight: 500;
}
.ring {
  fill: none;
  stroke: var(--color-accent);
  stroke-width: 3;
  pointer-events: none;
}
.focus {
  fill: none;
  stroke: transparent;
  stroke-width: 2;
  pointer-events: none;
}
.tile:focus-visible .focus {
  stroke: var(--color-accent);
}
</style>
