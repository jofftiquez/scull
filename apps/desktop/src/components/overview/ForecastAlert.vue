<script setup lang="ts">
// Forecast alert (design 01, top of the right sidebar): magenta tint, a ring counting the days
// left out of 30, one sentence and a Reclaim link to Cleanup Review.
import { formatBytes } from '../../lib/format';
import { daysDash } from './forecast';

// `drive` names the drive when there is more than one (D-22: one forecast per drive).
defineProps<{ days: number; safeBytes: number; drive?: string | null }>();
</script>

<template>
  <div role="status" class="flex items-center gap-3.5 rounded-[20px] border border-accent-tint-border bg-accent-tint px-4 py-3.5" data-testid="forecast">
    <svg width="44" height="44" viewBox="0 0 44 44" aria-hidden="true" class="flex-none">
      <circle cx="22" cy="22" r="18" fill="none" stroke="#F7D9E8" stroke-width="5" />
      <circle
        cx="22"
        cy="22"
        r="18"
        fill="none"
        stroke="#D6268A"
        stroke-width="5"
        stroke-linecap="round"
        :stroke-dasharray="`${daysDash(days)} 113`"
        transform="rotate(-90 22 22)"
        class="ring"
      />
      <text x="22" y="26" text-anchor="middle" font-size="12" font-weight="700" fill="#15121A">{{ days }}d</text>
    </svg>
    <span class="flex-1 text-label text-[#3A3440]">
      <strong class="text-ink">{{ drive ?? 'Disk' }} full in about {{ days }} day{{ days === 1 ? '' : 's' }}.</strong>
      {{ safeBytes > 0 ? `${formatBytes(safeBytes)} is safe to free now.` : 'Nothing is safe to free right now.' }}
    </span>
    <RouterLink
      to="/cleanup"
      class="inline-flex min-h-11 flex-none items-center rounded-small bg-ink px-3.5 text-label font-semibold text-on-ink no-underline hover:text-on-ink"
    >
      Reclaim
    </RouterLink>
  </div>
</template>

<style scoped>
.ring {
  transition: stroke-dasharray 300ms ease;
}
text {
  font-family: var(--font-sans);
}
</style>
