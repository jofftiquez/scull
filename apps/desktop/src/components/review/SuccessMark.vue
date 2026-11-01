<script setup lang="ts">
// Ring + check that draw themselves (design 03b/12d): ring 700ms, check 380ms after 620ms,
// optional confetti burst. `animate: false` shows the end state (e.g. a state reloaded later).
import { computed } from 'vue';

const props = withDefaults(defineProps<{ size?: number; animate?: boolean; burst?: boolean; track?: string; label?: string }>(), {
  size: 88,
  animate: true,
  burst: false,
  track: 'var(--color-surface-raised)',
  label: 'Done',
});

const COLORS = ['var(--color-accent)', 'var(--color-ink)', 'var(--color-tier-s0)', 'var(--color-tier-s2)', 'var(--color-tier-s1)'];
const particles = computed(() =>
  props.burst && props.animate
    ? Array.from({ length: 20 }, (_, i) => ({
        size: [8, 10, 12, 9][i % 4],
        radius: i % 3 === 0 ? '3px' : '50%',
        color: COLORS[i % COLORS.length],
        animation: `scull-b${i % 10} ${800 + (i % 5) * 90}ms ${500 + (i >= 10 ? 140 : 0)}ms cubic-bezier(.15,.7,.3,1) both`,
      }))
    : [],
);
</script>

<template>
  <span class="relative inline-flex flex-none">
    <span v-if="particles.length" aria-hidden="true" class="pointer-events-none absolute top-1/2 left-1/2 h-0 w-0">
      <span
        v-for="(p, i) in particles"
        :key="i"
        class="absolute -top-[5px] -left-[5px] opacity-0"
        :style="{ width: `${p.size}px`, height: `${p.size}px`, borderRadius: p.radius, background: p.color, animation: p.animation }"
      />
    </span>
    <svg :width="size" :height="size" viewBox="0 0 88 88" role="img" :aria-label="label">
      <circle cx="44" cy="44" r="34" fill="none" :stroke="track" stroke-width="8" />
      <circle
        cx="44"
        cy="44"
        r="34"
        fill="none"
        stroke="var(--color-tier-s0)"
        stroke-width="8"
        stroke-linecap="round"
        stroke-dasharray="214"
        :stroke-dashoffset="animate ? 214 : 0"
        transform="rotate(-90 44 44)"
        :style="animate ? 'animation: scull-ring 700ms ease-out forwards' : undefined"
      />
      <path
        d="M30 45 l9 9 l19 -20"
        fill="none"
        stroke="var(--color-ink)"
        stroke-width="6"
        stroke-linecap="round"
        stroke-linejoin="round"
        stroke-dasharray="48"
        :stroke-dashoffset="animate ? 48 : 0"
        :style="animate ? 'animation: scull-check 380ms 620ms ease-out forwards' : undefined"
      />
    </svg>
  </span>
</template>
