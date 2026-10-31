<script setup lang="ts">
// Tier chip (design 03): the label names the tier and S2/S3 carry stripes, so the tier never
// depends on color alone. The reason is the accessible description and the tooltip.
import { computed } from 'vue';
import type { Tier } from '../../bridge';
import { TIER_LABEL } from './plan';

const props = defineProps<{ tier: Tier; reason?: string }>();
const key = computed(() => props.tier.toLowerCase());
</script>

<template>
  <span
    class="inline-block whitespace-nowrap rounded-chip px-2 py-1 font-mono text-[11px] leading-none font-medium"
    :style="{ background: `var(--tier-${key}-fill)`, color: `var(--color-tier-${key}-text)` }"
    :title="reason"
    :data-tier="tier"
  >{{ TIER_LABEL[tier] }}<span v-if="reason" class="sr-only">: {{ reason }}</span></span>
</template>
