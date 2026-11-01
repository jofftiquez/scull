<script setup lang="ts">
// Ecosystem avatar (design 06): initials on a tinted square.
import { computed } from 'vue';
import type { ProjectSummary } from '../../bridge';
import { avatar } from './model';

const props = defineProps<{ project: Pick<ProjectSummary, 'ecosystems'>; size?: 'md' | 'lg' }>();
const a = computed(() => avatar(props.project));
</script>

<template>
  <span
    aria-hidden="true"
    class="flex flex-none items-center justify-center font-mono font-medium"
    :class="[size === 'lg' ? 'size-12 rounded-[14px] text-body' : 'size-10 rounded-button text-caption', `tone-${a.tone}`]"
  >{{ a.initials }}</span>
</template>

<style scoped>
.tone-js {
  background: var(--color-accent-tint);
  color: var(--color-accent-text);
}
.tone-rust {
  background: var(--color-violet-tint);
  color: var(--color-violet-text);
}
.tone-ml {
  background: #fff1dc;
  color: #8a5200;
}
.tone-game {
  background: var(--color-category-ring);
  color: var(--color-tier-s3);
}
.tone-other {
  background: var(--color-surface-raised);
  color: var(--color-ink);
}
</style>
