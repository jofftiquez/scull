<script setup lang="ts">
// App shell (D-18): paper ground, 12px gaps, full-height sidebar, then the current screen's
// main column (and its optional right column). Routes with `meta.bare` (onboarding) take the
// whole window.
import { onMounted } from 'vue';
import { useRoute, useRouter } from 'vue-router';
import AppSidebar from './components/shell/AppSidebar.vue';
import { useBridge } from './bridge';
import { shell } from './state/shell';
import { core, refresh } from './state/core';
import { needsOnboarding } from './components/onboarding/model';

const bridge = useBridge();
const route = useRoute();
const router = useRouter();
onMounted(async () => {
  try {
    shell.version = (await bridge.appInfo()).version;
  } catch {
    // The chip keeps its default; nothing else depends on it.
  }
  await refresh(bridge);
  // First open (PRD §14.7): start on onboarding instead of an empty overview.
  if (route.path === '/' && needsOnboarding(core.status)) await router.replace('/onboarding');
});
</script>

<template>
  <div class="flex min-h-screen flex-wrap items-stretch gap-page-gap bg-ground p-page-padding text-body text-ink">
    <AppSidebar v-if="!route.meta.bare" :version="shell.version" :counts="shell.counts" />
    <RouterView />
  </div>
</template>
