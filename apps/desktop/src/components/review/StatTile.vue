<script setup lang="ts">
// Figure tile (design 03 top row): label, big tabular number, one line of context.
defineProps<{ label: string; value: string; sub: string; tone?: 'accent' | 'muted' }>();
</script>

<template>
  <section
    :aria-label="label"
    class="tile col-span-12 flex flex-col gap-1.5 rounded-tile bg-surface p-tile-padding-sm sm:col-span-6 xl:col-span-3"
  >
    <span class="text-label font-medium text-text-secondary">{{ label }}</span>
    <span
      class="text-[36px] leading-[1.1] font-semibold tracking-[-0.03em] tabular-nums"
      :class="tone === 'accent' ? 'text-violet-light' : tone === 'muted' ? 'text-text-secondary' : 'text-ink'"
      aria-live="polite"
    >{{ value }}</span>
    <span class="text-label text-text-secondary">{{ sub }}</span>
    <slot />
  </section>
</template>
