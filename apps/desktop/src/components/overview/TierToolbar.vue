<script setup lang="ts">
// Floating tier-filter toolbar under the wheel (D-19; design 01): All, the four tiers, Clear.
// Each tier button carries its name next to the swatch, so the filter never relies on color.
import { TIERS, TIER_KEYS, type TierFilter } from './tiers';

defineProps<{ modelValue: TierFilter }>();
const emit = defineEmits<{ 'update:modelValue': [TierFilter]; clear: [] }>();

const ALL_SWATCH =
  'conic-gradient(var(--color-tier-s0-swatch) 0 25%, var(--color-tier-s1-swatch) 0 50%, var(--color-tier-s2-swatch) 0 75%, var(--color-tier-s3-swatch) 0)';
const options: { id: TierFilter; label: string; swatch: string }[] = [
  { id: 'all', label: 'All', swatch: ALL_SWATCH },
  ...TIER_KEYS.map((t) => ({ id: t, label: TIERS[t].label, swatch: TIERS[t].swatch })),
];
</script>

<template>
  <div
    role="toolbar"
    aria-label="Wheel tools"
    class="flex flex-wrap items-center justify-center gap-1 rounded-chip border border-line bg-surface-raised p-0.5"
    data-testid="tier-toolbar"
  >
    <button
      v-for="o in options"
      :key="o.id"
      type="button"
      :aria-pressed="modelValue === o.id"
      class="tool inline-flex min-h-11 cursor-pointer items-center gap-2 rounded-chip border-0 px-3.5 text-label font-medium"
      :class="modelValue === o.id ? 'bg-ink text-on-ink' : 'bg-transparent text-text-strong-muted'"
      :data-filter="o.id"
      @click="emit('update:modelValue', o.id)"
    >
      <span class="size-2.5 rounded-full" :style="{ background: o.swatch }" aria-hidden="true"></span>{{ o.label }}
    </button>
    <span class="mx-1.5 h-5 w-px bg-border" aria-hidden="true"></span>
    <button
      type="button"
      class="tool inline-flex min-h-11 min-w-11 cursor-pointer items-center justify-center rounded-chip border-0 bg-transparent px-3 text-label text-text-strong-muted"
      data-filter="clear"
      @click="emit('clear')"
    >
      Clear
    </button>
  </div>
</template>

<style scoped>
.tool {
  transition:
    background 200ms ease,
    color 200ms ease;
}
</style>
