<script setup lang="ts">
// One setting (design 10): title and explanation on the left, the control on the right, an
// optional origin (`default`, `config.toml`, `env SCULL_X`) just before it.
defineProps<{ title: string; description?: string; origin?: string; unavailable?: boolean }>();
</script>

<template>
  <fieldset :disabled="unavailable" :aria-label="title" class="m-0 flex min-w-0 flex-wrap items-center gap-4 border-0 border-t border-solid border-line px-0 py-3.5" data-testid="setting-row">
    <span class="flex min-w-[220px] flex-1 flex-col gap-0.5">
      <span class="font-semibold">{{ title }}</span>
      <span v-if="description" class="text-label text-text-secondary"><slot name="description">{{ description }}</slot></span>
      <span v-if="unavailable" class="text-caption text-text-secondary">Not available yet</span>
    </span>
    <span v-if="origin" class="font-mono text-[11px] text-text-tertiary" data-testid="origin">{{ origin }}</span>
    <slot />
  </fieldset>
</template>
