<script setup lang="ts">
// Horizontal tier bar (design 06): S0 solid violet, S1 lilac, S2 amber stripes, S3 plum hatch.
// Tiers are never told by color alone: S2/S3 carry patterns and the bar has a text label.
import { computed } from 'vue';
import type { BarSegment } from './model';
import { TIER_LABEL } from './model';

const props = defineProps<{ segments: BarSegment[] }>();
const label = computed(() =>
  props.segments
    .filter((s) => s.pct > 0)
    .map((s) => `${TIER_LABEL[s.tier]} ${Math.round(s.pct)}%`)
    .join(', ') || 'No candidates',
);
</script>

<template>
  <span class="flex h-2 gap-0.5 overflow-hidden rounded-chip bg-surface-raised" role="img" :aria-label="label">
    <span
      v-for="s in segments"
      :key="s.tier"
      class="seg h-full"
      :class="`seg-${s.tier.toLowerCase()}`"
      :style="{ width: `${s.pct.toFixed(1)}%` }"
    ></span>
  </span>
</template>

<style scoped>
.seg {
  transition: width 500ms ease;
}
.seg-s0 {
  background: var(--tier-s0-fill);
}
.seg-s1 {
  background: var(--tier-s1-fill);
}
.seg-s2 {
  background: repeating-linear-gradient(135deg, #ffb547 0 3px, #ffd08a 3px 6px);
}
.seg-s3 {
  background: var(--tier-s3-fill);
}
</style>
