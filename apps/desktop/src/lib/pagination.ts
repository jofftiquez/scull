import { computed, ref, type Ref } from 'vue';

export const PAGE_SIZE = 25;

export function usePagination<T>(items: Ref<readonly T[]>) {
  const page = ref(1);
  const pages = computed(() => Math.max(1, Math.ceil(items.value.length / PAGE_SIZE)));
  const current = computed(() => Math.min(Math.max(1, page.value), pages.value));
  const rows = computed(() => items.value.slice((current.value - 1) * PAGE_SIZE, current.value * PAGE_SIZE));
  return { page, current, pages, rows };
}
