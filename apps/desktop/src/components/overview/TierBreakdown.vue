<script setup lang="ts">
// "By safety tier" (design 01, bottom of the right sidebar): one row per tier with its chip
// (label + pattern), what it means, its on-disk bytes and a bar against the total.
import { computed } from 'vue';
import { formatBytes } from '../../lib/format';
import { TIERS, TIER_KEYS } from './tiers';
import type { WheelModel } from './wheel';

const props = defineProps<{ model: WheelModel | null }>();

const CHIP: Record<string, string> = {
  S0: 'var(--tier-s0-fill)',
  S1: 'var(--tier-s1-fill)',
  S2: 'var(--tier-s2-fill)',
  S3: 'var(--color-tier-s3)',
};
const BAR: Record<string, string> = { S0: '#7C4DFF', S1: '#DCCBFF', S2: '#FFB547', S3: '#3B2A4F' };

const total = computed(() => (props.model ? TIER_KEYS.reduce((a, t) => a + props.model!.byTier[t].bytes, 0) : 0));
const pct = (t: (typeof TIER_KEYS)[number]) =>
  total.value > 0 ? `${((props.model!.byTier[t].bytes / total.value) * 100).toFixed(1)}%` : '0%';
</script>

<template>
  <section aria-label="By safety tier" class="flex flex-col gap-3.5 rounded-tile-lg bg-surface p-tile-padding-sm" data-testid="tiers">
    <div class="flex justify-between">
      <span class="text-label font-medium text-text-tertiary">By safety tier</span>
      <span class="font-mono text-caption text-text-tertiary">{{ model ? formatBytes(total) : '—' }}</span>
    </div>
    <div v-for="t in TIER_KEYS" :key="t" class="flex flex-col gap-1.5" :data-tier="t">
      <div class="flex items-center gap-2.5">
        <span class="rounded-chip px-2.5 py-0.5 text-caption font-semibold" :style="{ background: CHIP[t], color: TIERS[t].badgeFg }">{{ TIERS[t].chip }}</span>
        <span class="flex-1 text-label text-text-secondary">{{ TIERS[t].hint }}</span>
        <span class="font-mono">{{ model ? formatBytes(model.byTier[t].bytes) : '—' }}</span>
      </div>
      <div class="h-1 rounded-chip bg-surface-raised">
        <div class="bar h-full rounded-chip" :style="{ width: model ? pct(t) : '0%', background: BAR[t] }"></div>
      </div>
    </div>
  </section>
</template>

<style scoped>
.bar {
  transition: width 300ms ease;
}
</style>
