<script setup lang="ts">
// Every preview field of one plan item (PRD §10.2, SAFE-01), shown under its row: path and
// project, artifact and evidence, the three sizes with confidence, tier and every reason,
// regeneration, risk flags, and the action with whether it can be restored.
import { computed } from 'vue';
import type { PlanItem } from '../../bridge/plan';
import { formatBytes } from '../../lib/format';
import TierBadge from './TierBadge.vue';
import { actionLabel, describeEvidence, displayPath, flagList, regenCost, restorability } from './plan';

const props = defineProps<{ item: PlanItem }>();
const c = computed(() => props.item.candidate);
const sizes = computed(() => c.value.sizes);
</script>

<template>
  <dl
    class="m-0 grid grid-cols-1 gap-x-6 gap-y-3 border-t border-line bg-surface-subtle px-6 py-4 text-label md:grid-cols-2 xl:grid-cols-3"
    data-testid="item-details"
  >
    <div class="flex flex-col gap-0.5">
      <dt class="text-caption font-semibold text-text-secondary">Exact path</dt>
      <dd class="m-0 font-mono [overflow-wrap:anywhere]">{{ c.path }}</dd>
      <dd v-if="item.members" class="m-0 text-caption text-text-secondary">{{ item.members }} folders under this path</dd>
    </div>
    <div class="flex flex-col gap-0.5">
      <dt class="text-caption font-semibold text-text-secondary">Project</dt>
      <dd class="m-0">
        <template v-if="c.project">{{ c.project.name }} · {{ c.project.ecosystem }} · <span class="font-mono">{{ displayPath(c.project.root) }}</span></template>
        <template v-else>No owning project</template>
      </dd>
    </div>
    <div class="flex flex-col gap-0.5">
      <dt class="text-caption font-semibold text-text-secondary">Artifact</dt>
      <dd class="m-0">{{ c.artifact }} <span class="font-mono text-caption text-text-secondary">· rule {{ c.rule.id }} v{{ c.rule.version }}</span></dd>
    </div>
    <div class="flex flex-col gap-0.5">
      <dt class="text-caption font-semibold text-text-secondary">Size</dt>
      <dd class="m-0 tabular-nums">
        logical {{ formatBytes(sizes.logical) }} · allocated {{ formatBytes(sizes.allocated) }} · reclaimable
        <strong>{{ formatBytes(sizes.reclaimable.bytes) }}</strong> ({{ sizes.reclaimable.confidence }} confidence)
      </dd>
      <dd v-if="sizes.reclaimable.shared_bytes" class="m-0 text-caption text-text-secondary">
        {{ formatBytes(sizes.reclaimable.shared_bytes) }} shared with other links is not freed
      </dd>
    </div>
    <div class="flex flex-col gap-0.5">
      <dt class="text-caption font-semibold text-text-secondary">Tier and reason</dt>
      <dd v-for="(r, i) in c.reasons" :key="i" class="m-0 flex items-start gap-2">
        <TierBadge :tier="r.tier" />
        <span>{{ r.message }} <span class="font-mono text-caption text-text-secondary">{{ r.code }}</span></span>
      </dd>
    </div>
    <div class="flex flex-col gap-0.5">
      <dt class="text-caption font-semibold text-text-secondary">Regeneration</dt>
      <dd class="m-0">{{ c.regeneration.instructions ?? 'No instructions known' }} · {{ regenCost(c) }}</dd>
    </div>
    <div class="flex flex-col gap-0.5">
      <dt class="text-caption font-semibold text-text-secondary">Evidence</dt>
      <dd class="m-0">
        <ul class="m-0 list-none p-0">
          <li v-for="(e, i) in c.evidence" :key="i">{{ describeEvidence(e) }}</li>
        </ul>
      </dd>
    </div>
    <div class="flex flex-col gap-0.5">
      <dt class="text-caption font-semibold text-text-secondary">Flags</dt>
      <dd class="m-0 flex flex-wrap gap-x-3 gap-y-0.5">
        <span v-for="f in flagList(c.flags)" :key="f.label">
          {{ f.label }}: <strong :class="f.warn ? 'text-accent-text' : 'font-normal'">{{ f.value }}</strong>
        </span>
      </dd>
    </div>
    <div class="flex flex-col gap-0.5">
      <dt class="text-caption font-semibold text-text-secondary">Action</dt>
      <dd class="m-0"><strong>{{ actionLabel(c.action) }}</strong> · {{ restorability(c) }}</dd>
    </div>
  </dl>
</template>
