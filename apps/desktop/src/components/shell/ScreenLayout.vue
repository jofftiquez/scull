<script setup lang="ts">
// The middle and right columns of a screen (D-18): the visual centerpiece in <main>, optional
// secondary information in a right <aside>. The sidebar is the app shell's.
import AppHeader from './AppHeader.vue';
import { useShellStatus } from '../../state/shell';

defineProps<{ asideLabel?: string }>();
const status = useShellStatus();
</script>

<template>
  <main id="content" tabindex="-1" class="flex min-w-0 flex-[999_1_600px] flex-col gap-page-gap outline-none">
    <AppHeader :status="status">
      <template #actions><slot name="actions" /></template>
    </AppHeader>
    <slot />
  </main>
  <aside
    v-if="$slots.aside"
    :aria-label="asideLabel ?? 'Details'"
    class="flex min-w-0 flex-[1_1_340px] flex-col gap-page-gap"
  >
    <slot name="aside" />
  </aside>
</template>
