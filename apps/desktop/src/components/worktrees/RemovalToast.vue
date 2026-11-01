<script setup lang="ts">
// Toast after a removal (design 04b): ink pill, violet check that draws itself, the freed size.
defineProps<{ text: string; amount: string }>();
</script>

<template>
  <div
    role="status"
    class="flex items-center gap-3 rounded-chip bg-ink px-4 py-3.5 text-on-ink"
    style="animation: scull-rise 360ms cubic-bezier(.2,.9,.3,1.2)"
    data-testid="toast"
  >
    <svg width="28" height="28" viewBox="0 0 88 88" aria-hidden="true" class="flex-none">
      <circle cx="44" cy="44" r="40" fill="var(--color-tier-s0)" />
      <path
        d="M28 45 l11 11 l21 -23"
        fill="none"
        stroke="var(--color-on-fill)"
        stroke-width="9"
        stroke-linecap="round"
        stroke-linejoin="round"
        stroke-dasharray="48"
        stroke-dashoffset="48"
        style="animation: scull-check 360ms 200ms ease-out forwards"
      />
    </svg>
    <span class="flex-1 text-label">{{ text }}</span>
    <span class="font-mono text-label text-on-ink-violet">{{ amount }}</span>
  </div>
</template>
