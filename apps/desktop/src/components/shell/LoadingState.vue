<script setup lang="ts">
withDefaults(defineProps<{ label?: string }>(), { label: 'Loading…' });
</script>

<template>
  <div class="loading-state" role="status" data-testid="loading-state">
    <div class="loading-heading"><span class="loading-spinner" aria-hidden="true" /><span>{{ label }}</span></div>
    <div class="loading-lines" aria-hidden="true"><span /><span /><span /></div>
  </div>
</template>

<style scoped>
.loading-state { display: flex; flex-direction: column; justify-content: center; gap: var(--spacing-tile-padding); min-height: 240px; padding: var(--spacing-tile-padding); border: 1px solid var(--color-line); border-radius: var(--radius-card); background: var(--color-surface); color: var(--color-text-secondary); }
.loading-heading { display: flex; align-items: center; gap: 12px; }
.loading-spinner { width: 20px; height: 20px; border: 2px solid var(--color-accent-track); border-top-color: var(--color-accent); border-radius: 50%; animation: scull-spin 800ms linear infinite; }
.loading-lines { display: flex; flex-direction: column; gap: 12px; }
.loading-lines span { width: 100%; height: 12px; border-radius: var(--radius-small); background: linear-gradient(90deg, var(--color-surface-raised) 25%, var(--color-line) 50%, var(--color-surface-raised) 75%); background-size: 200% 100%; animation: loading-shimmer 1.6s ease-in-out infinite; }
.loading-lines span:nth-child(2) { width: 78%; }
.loading-lines span:nth-child(3) { width: 56%; }
@keyframes loading-shimmer { from { background-position: 200% 0; } to { background-position: -200% 0; } }
@media (prefers-reduced-motion: reduce) { .loading-spinner, .loading-lines span { animation: none; } }
</style>
