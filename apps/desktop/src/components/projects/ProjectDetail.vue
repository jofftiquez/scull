<script setup lang="ts">
import ActionBar from '../shell/ActionBar.vue';
import ActionFooter from '../shell/ActionFooter.vue';
// Project details (design 06 right column): totals, what's inside by tier, how it was found,
// and "Clean regenerable" with its fill animation and the success toast (06b).
import { computed } from 'vue';
import type { ProjectSummary } from '../../bridge';
import { formatBytes } from '../../lib/format';
import ProjectAvatar from './ProjectAvatar.vue';
import TierChip from './TierChip.vue';
import { ecosystemLabel, inside, lastActivity, projectSize, regenerable, shortPath, type CleanState } from './model';

const props = defineProps<{
  project: ProjectSummary;
  clean: CleanState;
  /** Regenerable bytes were cleaned here in this session. */
  cleaned: boolean;
  now: number;
  toast: { text: string; bytes: number } | null;
  note: string | null;
  revealLabel: string;
}>();
defineEmits<{ clean: []; neverSuggest: []; reveal: [] }>();

const s0 = computed(() => regenerable(props.project));
const rows = computed(() => inside(props.project));
const busy = computed(() => props.clean.phase === 'cleaning');
const found = computed(() => {
  const p = props.project;
  const parts = [];
  if (p.markers.length) parts.push(`Found by ${p.markers.join(', ')}`);
  if (p.git_worktree) parts.push(p.git_worktree === p.root ? 'Git repository' : `in Git worktree ${shortPath(p.git_worktree)}`);
  parts.push(`last activity ${lastActivity(p.newest_mtime, props.now)}`);
  return parts.join(' · ');
});
</script>

<template>
  <section aria-live="polite" class="flex flex-col gap-3.5 rounded-tile bg-surface p-tile-padding" data-testid="detail">
    <div class="flex items-center gap-3">
      <ProjectAvatar :project="project" size="lg" />
      <div class="flex min-w-0 flex-col">
        <h2 class="m-0 truncate text-h2 font-semibold">{{ project.name }}</h2>
        <span class="truncate font-mono text-caption text-text-tertiary">{{ shortPath(project.root) }}</span>
      </div>
    </div>
    <div class="flex flex-wrap gap-1.5">
      <span v-for="e in project.ecosystems" :key="e" class="rounded-chip bg-surface-raised px-2.5 py-1 text-caption font-medium">{{ ecosystemLabel(e) }}</span>
    </div>
    <div class="grid grid-cols-2 gap-2">
      <div class="flex flex-col rounded-card bg-ground p-3.5">
        <span class="text-caption text-text-tertiary">Total</span>
        <span class="text-stat-sm font-bold tabular-nums" data-testid="detail-total">{{ formatBytes(projectSize(project)) }}</span>
      </div>
      <div class="flex flex-col rounded-card bg-violet-tint p-3.5">
        <span class="text-caption text-violet-text">Reclaimable</span>
        <span class="text-stat-sm font-bold text-violet-text tabular-nums" data-testid="detail-reclaim">{{ formatBytes(s0) }}</span>
      </div>
    </div>

    <div class="flex flex-col">
      <h3 class="m-0 pb-1.5 text-label font-medium text-text-tertiary">What's inside</h3>
      <div v-if="cleaned" class="flex items-center gap-2.5 border-t border-line py-2.5 opacity-40" data-testid="inside-cleaned">
        <span class="flex-1 text-label">regenerable items</span>
        <TierChip tier="S0" />
        <span class="w-[76px] text-right font-mono text-label">cleaned</span>
      </div>
      <div
        v-for="r in rows"
        :key="r.label"
        class="flex items-center gap-2.5 border-t border-line py-2.5"
        :class="r.tier === 'S0' && cleaned ? 'opacity-40' : ''"
        style="transition: opacity 400ms ease"
        data-testid="inside-row"
      >
        <span class="flex-1 text-label">{{ r.tier ? r.label.toLowerCase() + ' items' : 'other files' }}</span>
        <TierChip v-if="r.tier" :tier="r.tier" />
        <span v-else class="rounded-chip border border-line px-2 py-0.5 text-[11px] font-semibold text-text-tertiary">Not offered</span>
        <span class="w-[76px] text-right font-mono text-label">{{ formatBytes(r.bytes) }}</span>
      </div>
      <p v-if="rows.length === 0 && !cleaned" class="m-0 border-t border-line py-2.5 text-label text-text-secondary">Size unknown.</p>
    </div>

    <p class="m-0 text-caption text-text-secondary">{{ found }}</p>

    <p v-if="note" class="m-0 text-caption text-text-secondary" role="status">{{ note }}</p>
    <ActionFooter panel="aside" aria-label="Project actions">
      <ActionBar :summary="`${project.name} · ${formatBytes(s0)} regenerable`" label="Review cleanup" :busy="busy" :disabled="s0 === 0" data-testid="clean" @action="$emit('clean')" />
    </ActionFooter>
  </section>
</template>

<style scoped>
.clean-fill {
  background: repeating-linear-gradient(135deg, var(--color-tier-s0) 0 12px, var(--color-violet-light) 12px 24px);
  background-size: 48px 100%;
  animation: scull-shimmer 600ms linear infinite;
  transition: width 120ms linear;
}
.btn-line {
  flex: 1;
  min-height: 44px;
  border: 1px solid var(--color-border);
  border-radius: var(--radius-button);
  background: var(--color-surface);
  color: var(--color-ink);
  font: inherit;
  font-size: 13px;
  font-weight: 500;
  cursor: pointer;
}
.btn-line:hover {
  background: var(--color-surface-subtle);
}
.toast {
  animation: scull-rise 360ms cubic-bezier(0.2, 0.9, 0.3, 1.2);
}
.check {
  stroke-dasharray: 48;
  stroke-dashoffset: 48;
  animation: proj-check 360ms 200ms ease-out forwards;
}
@keyframes proj-check {
  to {
    stroke-dashoffset: 0;
  }
}
</style>
