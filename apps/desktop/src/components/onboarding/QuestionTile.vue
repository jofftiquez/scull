<script setup lang="ts">
// One onboarding question tile (design 02): "01" in mono, the question, an optional hint.
defineProps<{ n: string; title: string; hint?: string; gap?: 'sm' | 'md' }>();
</script>

<template>
  <section
    :aria-labelledby="`q${n}`"
    class="flex flex-col rounded-tile bg-surface p-tile-padding"
    :class="gap === 'md' ? 'gap-4' : 'gap-3'"
  >
    <div class="flex items-baseline gap-3">
      <span class="font-mono text-caption text-text-secondary">{{ n }}</span>
      <h2 :id="`q${n}`" class="m-0 text-h2 font-semibold">{{ title }}</h2>
      <span v-if="hint" class="text-label text-text-secondary">{{ hint }}</span>
    </div>
    <slot />
  </section>
</template>
