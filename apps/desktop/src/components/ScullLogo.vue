<script setup lang="ts">
// The scull mark (D-21): skull over crossed oars, line icon; magenta blades, violet eyes.
// `wink` plays the logo wink once (success moments, D-20); reduced motion collapses it.
defineProps<{ size?: number; wink?: boolean }>();
</script>

<template>
  <svg
    :width="size ?? 30"
    :height="size ?? 30"
    viewBox="0 0 32 32"
    fill="none"
    stroke="var(--color-ink)"
    stroke-width="2"
    stroke-linecap="round"
    stroke-linejoin="round"
    role="img"
    aria-label="scull logo"
  >
    <line x1="5" y1="27" x2="27" y2="5" />
    <line x1="5" y1="5" x2="27" y2="27" />
    <line x1="3.5" y1="28.5" x2="7.5" y2="24.5" stroke-width="4.5" stroke="var(--color-accent)" />
    <line x1="28.5" y1="28.5" x2="24.5" y2="24.5" stroke-width="4.5" stroke="var(--color-accent)" />
    <path d="M10 15a6 6 0 0 1 12 0v3l-2 2v2h-8v-2l-2-2z" fill="var(--color-surface)" />
    <circle cx="13.6" cy="15.4" r="1.4" fill="var(--color-tier-s0)" stroke="none" />
    <circle
      cx="18.4"
      cy="15.4"
      r="1.4"
      fill="var(--color-tier-s0)"
      stroke="none"
      :style="wink ? 'transform-box: fill-box; transform-origin: center; animation: scull-wink 520ms ease-in-out 1' : undefined"
    />
  </svg>
</template>
