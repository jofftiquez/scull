<script setup lang="ts">
// "Safe to reclaim" hero (design 01): the regenerable (S0) reclaimable bytes, big, in violet;
// a sentence built from the scan; the Reclaim button opens Cleanup Review (the desktop itself
// never deletes).
import { computed } from 'vue';
import { formatBytes } from '../../lib/format';
import { splitBytes } from './summary';

const props = defineProps<{ bytes: number; summary: string; scanned: boolean }>();
const parts = computed(() => splitBytes(props.bytes));
</script>

<template>
  <section aria-label="Safe to reclaim" class="flex flex-col gap-2.5 rounded-tile-lg bg-surface p-tile-padding" data-testid="hero">
    <div class="flex items-baseline gap-2 text-[#7C4DFF]">
      <span class="text-stat-xl leading-none font-bold tracking-[-0.04em] tabular-nums">{{ scanned ? parts[0] : '—' }}</span>
      <span v-if="scanned" class="text-[22px] font-semibold">{{ parts[1] }}</span>
    </div>
    <span class="text-[15px] text-ink">safe to reclaim now</span>
    <span class="text-label text-text-secondary">{{ scanned ? summary : 'Appears after the first scan.' }}</span>
    <RouterLink
      v-if="scanned && bytes > 0"
      to="/cleanup"
      class="mt-1.5 inline-flex min-h-12 items-center self-start rounded-button-lg bg-ink px-[22px] font-bold text-on-ink no-underline hover:text-on-ink"
    >
      Reclaim {{ formatBytes(bytes) }}
    </RouterLink>
    <RouterLink
      v-else-if="scanned"
      to="/cleanup"
      class="mt-1.5 inline-flex min-h-12 items-center self-start rounded-button-lg border border-border px-[22px] font-semibold text-ink no-underline"
    >
      Open Cleanup Review
    </RouterLink>
  </section>
</template>
