<script setup lang="ts">
// Navigation and drive status scroll independently from the page.
import { computed } from 'vue';
import { useRoute } from 'vue-router';
import ScullLogo from '../ScullLogo.vue';
import NavIcon from '../icons/NavIcon.vue';
import SearchField from './SearchField.vue';
import DrivesWidget from './DrivesWidget.vue';
import { NAV_ITEMS, type NavIconName } from '../../navigation';
import { shell } from '../../state/shell';

const props = defineProps<{
  /** Version shown in the chip next to the logo. */
  version: string;
  /** Short figures next to nav items (`41`, `14.9 GiB`); absent until a scan has run. */
  counts?: Partial<Record<NavIconName, string>>;
}>();

const route = useRoute();
const main = NAV_ITEMS.filter((i) => i.group === 'main');
const footer = NAV_ITEMS.filter((i) => i.group === 'footer');
const current = computed(() => route.name);
const versionLabel = computed(() => {
  const [major = '0', minor = '0'] = props.version.split('.');
  return `v${major}.${minor}`;
});
</script>

<template>
  <nav
    aria-label="Primary"
    class="app-sidebar bg-surface"
    data-testid="sidebar"
  >
    <div class="sidebar-brand flex items-center gap-3 px-6">
      <ScullLogo :key="shell.wink" :size="30" :wink="shell.wink > 0" />
      <span class="text-h2 font-semibold tracking-[-0.01em] text-ink">scull</span>
      <span
        class="ml-auto rounded-chip border border-border px-2 py-0.5 font-mono text-[11px] text-text-tertiary"
        :title="`scull ${version}`"
      >{{ versionLabel }}</span>
    </div>

    <div class="sidebar-scroll" tabindex="0" data-testid="sidebar-scroll">
      <template v-for="(group, gi) in [main, footer]" :key="gi">
        <div v-if="gi === 1" class="mx-2 my-2.5 h-px bg-line" role="separator"></div>
        <RouterLink
          v-for="item in group"
          :key="item.name"
          :to="item.path"
          class="nav-item flex min-h-touch items-center gap-3 rounded-button px-3 no-underline"
          :class="current === item.name ? 'bg-ink font-medium text-on-ink hover:text-on-ink' : 'text-text-strong-muted hover:bg-surface-subtle hover:text-ink'"
          :aria-current="current === item.name ? 'page' : undefined"
        >
          <NavIcon :name="item.name" :class="current === item.name ? 'text-accent' : ''" />
          <span>{{ item.label }}</span>
          <span
            v-if="counts?.[item.name]"
            class="ml-auto font-mono text-caption"
            :class="current === item.name ? 'text-on-ink' : 'text-text-tertiary'"
          >{{ counts[item.name] }}</span>
        </RouterLink>
      </template>

      <div class="mt-auto flex flex-col gap-2.5 pt-6">
        <SearchField />
        <DrivesWidget />
      </div>
    </div>
  </nav>
</template>

<style scoped>
.app-sidebar { display: flex; flex: 0 0 248px; flex-direction: column; min-height: 0; min-width: 0; height: 100%; overflow: hidden; border-right: 1px solid var(--color-line); }
.sidebar-brand { min-height: 72px; flex-shrink: 0; border-bottom: 1px solid var(--color-line); }
.sidebar-scroll { display: flex; flex: 1; flex-direction: column; gap: 4px; min-height: 0; padding: 16px; overflow: auto; overscroll-behavior: contain; scrollbar-gutter: stable; }
.sidebar-scroll:focus-visible { outline: 2px solid var(--color-accent); outline-offset: -2px; }
@media (max-width: 1150px) { .app-sidebar { flex-basis: 216px; } }
</style>
