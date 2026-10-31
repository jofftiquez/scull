<script setup lang="ts">
// Tier name chip (design 06 "What's inside"): fill and text by tier; S2 keeps its stripes.
import type { Tier } from '../../bridge';
import { TIER_LABEL } from './model';

defineProps<{ tier: Tier }>();
</script>

<template>
  <span class="chip rounded-chip px-2 py-0.5 text-[11px] font-semibold" :class="`chip-${tier.toLowerCase()}`">{{ TIER_LABEL[tier] }}</span>
</template>

<style scoped>
.chip-s0 {
  background: var(--color-tier-s0);
  color: var(--color-tier-s0-text);
}
.chip-s1 {
  background: var(--color-tier-s1);
  color: var(--color-tier-s1-text);
}
.chip-s2 {
  background: var(--tier-s2-fill);
  color: var(--color-tier-s2-text);
}
.chip-s3 {
  background: var(--color-tier-s3);
  color: var(--color-tier-s3-text);
}
</style>
