<script setup lang="ts">
// Full-height left sidebar (D-18): logo + navigation on top; search, supporter card and the
// drives widget pinned to the bottom. Proportions and colors follow design/html/Main.html
// (rendered at 1440px the design's sidebar is 278px wide: its 248px basis grows to fit).
import { computed } from 'vue';
import { useRoute } from 'vue-router';
import ScullLogo from '../ScullLogo.vue';
import NavIcon from '../icons/NavIcon.vue';
import SearchField from './SearchField.vue';
import SupporterCard from './SupporterCard.vue';
import DrivesWidget from './DrivesWidget.vue';
import { NAV_ITEMS, type NavIconName } from '../../navigation';

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
    class="flex flex-[1_1_278px] flex-col gap-1 rounded-tile-lg bg-surface p-4"
    data-testid="sidebar"
  >
    <div class="flex items-center gap-3 px-2 pt-1.5 pb-4">
      <ScullLogo :size="30" />
      <span class="text-h2 font-semibold tracking-[-0.01em] text-ink">scull</span>
      <span
        class="ml-auto rounded-chip border border-border px-2 py-0.5 font-mono text-[11px] text-text-tertiary"
        :title="`scull ${version}`"
      >{{ versionLabel }}</span>
    </div>

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
      <SupporterCard />
      <DrivesWidget />
    </div>
  </nav>
</template>
