<script setup lang="ts">
// Neutral empty tile for screens that are not built yet. White, rounded, no shadow (D-17).
defineProps<{ title: string; issue: string; note?: string }>();
</script>

<template>
  <section
    :aria-label="title"
    class="flex flex-1 flex-col items-center justify-center gap-2 rounded-[28px] bg-surface p-tile-padding text-center"
    data-testid="placeholder"
  >
    <h1 class="m-0 text-h2 font-semibold text-ink">{{ title }}</h1>
    <p class="m-0 max-w-[44ch] text-body text-text-secondary">
      {{ note ?? 'This screen is part of the desktop app and is being built.' }}
    </p>
    <span class="font-mono text-caption text-text-tertiary">{{ issue }}</span>
  </section>
</template>
