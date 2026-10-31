<script setup lang="ts">
// App shell (D-18): paper ground, 12px gaps, full-height sidebar, then the current screen's
// main column (and its optional right column).
import { onMounted } from 'vue';
import AppSidebar from './components/shell/AppSidebar.vue';
import { useBridge } from './bridge';
import { shell } from './state/shell';

const bridge = useBridge();
onMounted(async () => {
  try {
    shell.version = (await bridge.appInfo()).version;
  } catch {
    // The chip keeps its default; nothing else depends on it.
  }
});
</script>

<template>
  <div class="flex min-h-screen flex-wrap items-stretch gap-page-gap bg-ground p-page-padding text-body text-ink">
    <AppSidebar :version="shell.version" :counts="shell.counts" />
    <RouterView />
  </div>
</template>
