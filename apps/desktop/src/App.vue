<script setup lang="ts">
// App shell (D-18): paper ground, 12px gaps, full-height sidebar, then the current screen's
// main column (and its optional right column). Routes with `meta.bare` (onboarding) take the
// whole window.
import { nextTick, onBeforeUnmount, onMounted, watch } from 'vue';
import { START_LOCATION, useRoute, useRouter } from 'vue-router';
import AppSidebar from './components/shell/AppSidebar.vue';
import AgentRequestNotice from './components/review/AgentRequestNotice.vue';
import { useBridge } from './bridge';
import { usePlanBridge } from './bridge/plan';
import { shell } from './state/shell';
import { core, refresh } from './state/core';
import { needsOnboarding } from './components/onboarding/model';
import type { NavItem } from './navigation';
import { createTauriWindowCommands } from './bridge/tauri';
import { isMainRoute } from './components/menubar/window';
import { monitorControlFor } from './monitor/control';
import { startMonitorSync } from './monitor/prefs';

const bridge = useBridge();
const plans = usePlanBridge();
const route = useRoute();
const router = useRouter();
// `scull://approve/<plan-id>` (Tauri): open Cleanup Review on that agent request. A link only
// navigates; approving still needs the review and the typed code.
const stopLinks = plans.onOpenRequest((planId) => {
  void router.push({ path: '/cleanup', query: { request: planId } });
});
onBeforeUnmount(stopLinks);
// WCAG 2.4.2 / 2.4.3: each screen has its own title, and after a navigation focus moves to the
// new screen's <main> so keyboard and screen-reader users start there (not on the old link).
watch(
  () => route.path,
  () => {
    const item = route.meta.item as NavItem | undefined;
    document.title = item ? `${item.label} · scull` : route.name === 'onboarding' ? 'Welcome · scull' : 'scull';
  },
  { immediate: true },
);
// Not on the first load (focus starts at the top of the page), only on later navigations.
const stopFocusHook = router.afterEach((to, from, failure) => {
  if (!failure && from !== START_LOCATION && to.path !== from.path) void nextTick(skipToContent);
});
onBeforeUnmount(stopFocusHook);

/** Move focus to the screen's <main> (hash routing: the link must not change the URL). */
function skipToContent() {
  document.getElementById('content')?.focus();
}
// The menu bar widget (P4-UI-01) asks the tray to show this window on a screen; follow it.
let stopNavigate: (() => void) | null = null;
// Tray monitoring (P5-PRO-01): send the stored settings and memory, keep its snapshots.
let stopMonitor: (() => void) | null = null;
onBeforeUnmount(() => {
  stopNavigate?.();
  stopMonitor?.();
});
onMounted(async () => {
  void startMonitorSync(monitorControlFor(bridge))
    .then((stop) => (stopMonitor = stop))
    .catch(() => undefined);
  if (bridge.kind === 'tauri') {
    void createTauriWindowCommands()
      .onNavigate((to) => {
        if (isMainRoute(to)) void router.push(to);
      })
      .then((stop) => (stopNavigate = stop))
      .catch(() => undefined);
  }
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
    <!-- WCAG 2.4.1: the first Tab stop skips the sidebar's navigation. -->
    <a
      v-if="!route.meta.bare"
      href="#content"
      class="sr-only rounded-button bg-ink px-4 text-on-ink focus:not-sr-only focus:fixed focus:px-4 focus:top-3 focus:left-3 focus:z-50 focus:flex focus:min-h-touch focus:items-center"
      data-testid="skip-link"
      @click.prevent="skipToContent"
    >
      Skip to main content
    </a>
    <AppSidebar v-if="!route.meta.bare" :version="shell.version" :counts="shell.counts" />
    <RouterView />
    <AgentRequestNotice v-if="!route.meta.bare" />
  </div>
</template>
