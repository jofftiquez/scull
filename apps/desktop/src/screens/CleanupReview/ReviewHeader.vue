<script setup lang="ts">
// Header tile of Cleanup Review (design 03/12): title, optional "Agent request" pill, the
// plan line in mono, and the screen's actions.
defineProps<{ subline: string; agent?: boolean; pulse?: boolean }>();
</script>

<template>
  <header class="flex flex-wrap items-center justify-between gap-3 px-2 py-2">
    <div class="flex flex-col gap-1">
      <div class="flex flex-wrap items-center gap-3">
        <h1 class="m-0 text-h2 font-semibold">Cleanup Review</h1>
        <span
          v-if="agent"
          class="inline-flex items-center gap-2 rounded-chip bg-ink px-3 py-1 text-caption font-semibold text-on-ink"
          data-testid="agent-pill"
        >
          <span
            aria-hidden="true"
            class="h-2 w-2 rounded-chip bg-accent"
            :style="pulse ? 'animation: scull-pulse 1600ms ease-in-out infinite' : undefined"
          />Agent request
        </span>
      </div>
      <span class="font-mono text-caption text-text-secondary" data-testid="plan-line">{{ subline }}</span>
    </div>
    <div class="flex flex-wrap gap-2">
      <slot />
    </div>
  </header>
</template>
