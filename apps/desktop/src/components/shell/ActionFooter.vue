<script setup lang="ts">
import { computed, inject } from 'vue';
import { FOOTER_TARGETS } from './footer';

defineOptions({ inheritAttrs: false });
withDefaults(defineProps<{ panel?: 'main' | 'aside' }>(), { panel: 'main' });
const targets = inject(FOOTER_TARGETS, null);
const target = computed(() => targets?.value ?? null);
</script>

<template>
  <Teleport :to="target ?? 'body'" :disabled="!target">
    <footer role="region" :aria-label="panel === 'main' ? 'Page actions' : 'Detail actions'" v-bind="$attrs" class="action-footer"><slot /></footer>
  </Teleport>
</template>

<style scoped>
.action-footer {
  min-width: 0;
  padding: var(--spacing-tile-padding-sm);
  border-radius: 0;
  border-top: 1px solid var(--color-line);
}
</style>
