<script setup lang="ts">
import { computed } from 'vue';
import { PAGE_SIZE } from '../../lib/pagination';
const props = defineProps<{ total: number; page: number }>();
defineEmits<{ 'update:page': [page: number] }>();
const pages = computed(() => Math.max(1, Math.ceil(props.total / PAGE_SIZE)));
const current = computed(() => Math.min(Math.max(1, props.page), pages.value));
</script>

<template>
  <nav v-if="total" aria-label="List pagination" class="flex flex-wrap items-center justify-between gap-3 border-t border-line px-4 py-2 text-label">
    <span role="status" class="text-text-secondary">{{ (current - 1) * PAGE_SIZE + 1 }}–{{ Math.min(current * PAGE_SIZE, total) }} of {{ total }}</span>
    <span v-if="pages > 1" class="flex items-center gap-2">
      <button type="button" :disabled="current === 1" class="min-h-touch rounded-button border border-border bg-surface px-3 text-ink disabled:cursor-not-allowed disabled:opacity-45" @click="$emit('update:page', current - 1)">Previous</button>
      <span>Page {{ current }} of {{ pages }}</span>
      <button type="button" :disabled="current === pages" class="min-h-touch rounded-button border border-border bg-surface px-3 text-ink disabled:cursor-not-allowed disabled:opacity-45" @click="$emit('update:page', current + 1)">Next</button>
    </span>
  </nav>
</template>
