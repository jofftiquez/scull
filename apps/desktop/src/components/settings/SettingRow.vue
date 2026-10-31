<script setup lang="ts">
// One setting (design 10): title and explanation on the left, the control on the right, an
// optional origin (`default`, `config.toml`, `env SCULL_X`) just before it.
defineProps<{ title: string; description?: string; origin?: string }>();
</script>

<template>
  <div class="flex flex-wrap items-center gap-4 border-t border-line py-3.5" data-testid="setting-row">
    <span class="flex min-w-[220px] flex-1 flex-col gap-0.5">
      <span class="font-semibold">{{ title }}</span>
      <span v-if="description" class="text-label text-text-secondary"><slot name="description">{{ description }}</slot></span>
    </span>
    <span v-if="origin" class="font-mono text-[11px] text-text-tertiary" data-testid="origin">{{ origin }}</span>
    <slot />
  </div>
</template>
