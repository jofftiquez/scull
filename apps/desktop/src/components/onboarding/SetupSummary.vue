<script setup lang="ts">
// "Here's what scull will focus on" (design 02, ONB-03): the live summary of the answers, then
// the first scan's progress and result (02b), then the actions.
import { computed } from 'vue';
import { formatBytes } from '../../lib/format';
import type { FirstScan, SummaryRow } from './model';

const props = defineProps<{ rows: SummaryRow[]; scan: FirstScan; fill: number }>();

const shown = computed(() => props.scan.phase !== 'idle');
const entries = computed(() => (props.scan.result?.entries ?? props.scan.entries).toLocaleString('en-US'));
const statusText = computed(() => {
  switch (props.scan.phase) {
    case 'done':
      return 'Scan complete';
    case 'cancelled':
      return 'Scan stopped · partial';
    case 'failed':
      return 'Scan failed';
    default:
      return props.scan.activity || 'Starting…';
  }
});
</script>

<template>
  <section
    aria-label="Your setup"
    class="flex flex-wrap items-end gap-x-12 gap-y-6 rounded-tile border border-accent-tint-border bg-accent-tint p-8 text-ink"
    data-testid="summary"
  >
    <div class="flex flex-[1_1_520px] flex-col gap-4">
      <span class="text-label font-medium text-text-strong-muted">Here's what scull will focus on</span>
      <dl class="m-0 grid grid-cols-1 gap-1.5 min-[640px]:grid-cols-2">
        <div v-for="r in rows" :key="r.k" class="flex flex-col gap-1 rounded-[14px] bg-surface p-4" data-testid="summary-row">
          <dt class="font-mono text-[11px] text-text-strong-muted">{{ r.k }}</dt>
          <dd class="m-0 font-medium">{{ r.v }}</dd>
        </div>
      </dl>
    </div>

    <div v-if="shown" class="scan-rise flex flex-[1_1_100%] flex-col gap-3" data-testid="scan">
      <div class="flex flex-wrap items-baseline justify-between gap-3" aria-live="polite">
        <span class="text-[28px] font-semibold tracking-[-0.02em] tabular-nums" data-testid="entries">{{ entries }} entries</span>
        <span class="font-mono text-label text-text-strong-muted">{{ statusText }}</span>
      </div>
      <div
        class="h-3.5 overflow-hidden rounded-chip bg-surface"
        role="progressbar"
        aria-label="First scan"
        aria-valuemin="0"
        aria-valuemax="100"
        :aria-valuenow="Math.round(fill * 100)"
      >
        <div class="scan-fill h-full rounded-chip" :style="{ width: `${Math.round(fill * 100)}%` }"></div>
      </div>
      <div v-if="scan.result" class="flex flex-wrap gap-2" data-testid="found-chips">
        <span class="rounded-chip bg-surface px-3 py-1.5 text-label">{{ scan.result.projects }} project{{ scan.result.projects === 1 ? '' : 's' }}</span>
        <span class="rounded-chip bg-surface px-3 py-1.5 text-label">{{ scan.result.worktrees }} worktree{{ scan.result.worktrees === 1 ? '' : 's' }}</span>
        <span class="rounded-chip bg-tier-s0 px-3 py-1.5 text-label font-semibold text-tier-s0-text">{{ formatBytes(scan.result.safeBytes) }} safely reclaimable</span>
      </div>
      <p v-if="scan.error" class="m-0 text-label text-accent-text" role="alert">{{ scan.error }}</p>
    </div>

    <div class="flex flex-wrap gap-2">
      <slot />
    </div>
  </section>
</template>

<style scoped>
.scan-rise {
  animation: scull-rise 320ms ease-out;
}
.scan-fill {
  background: repeating-linear-gradient(135deg, var(--color-tier-s0) 0 12px, var(--color-violet-light) 12px 24px);
  background-size: 48px 100%;
  animation: scull-shimmer 600ms linear infinite;
  transition: width 120ms linear;
}
</style>
