<script setup lang="ts">
// Page header above the content (design 01): scan freshness on the left in mono, the page's
// actions on the right.
defineProps<{ status: string }>();
</script>

<template>
  <header class="flex flex-wrap items-center justify-between gap-3 px-2 pt-1.5" data-testid="header">
    <span class="font-mono text-caption text-text-tertiary" aria-live="polite">{{ status }}</span>
    <div class="flex flex-wrap gap-1.5">
      <slot name="actions" />
    </div>
  </header>
</template>
