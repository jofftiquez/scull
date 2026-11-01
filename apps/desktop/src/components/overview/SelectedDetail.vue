<script setup lang="ts">
// Selected segment (design 01): what it is, its tier badge, size, why it has that tier, and
// what you can do. Actions never delete: "Add to cleanup" marks it for Cleanup Review, "Review"
// opens Cleanup Review, "Why protected?" lists the reasons.
import { computed, ref, watch } from 'vue';
import { useRouter } from 'vue-router';
import { formatBytes } from '../../lib/format';
import { TIERS } from './tiers';
import { categoryReason, itemReason } from './summary';
import type { WheelCategory, WheelItem } from './wheel';

const props = defineProps<{ item: WheelItem | null; category: WheelCategory | null }>();
const emit = defineEmits<{ add: [WheelItem]; tiers: [] }>();
const router = useRouter();
const why = ref(false);
watch(
  () => [props.item, props.category],
  () => (why.value = false),
);

const view = computed(() => {
  if (props.item) {
    const t = TIERS[props.item.tier];
    return {
      label: 'Selected',
      name: props.item.name,
      size: formatBytes(props.item.bytes),
      reason: itemReason(props.item),
      tier: t,
      action: props.item.tier === 'S0' || props.item.tier === 'S1' ? 'Add to cleanup' : props.item.tier === 'S2' ? 'Review' : 'Why protected?',
      path: props.item.candidate?.path ?? null,
    };
  }
  if (props.category) {
    const t = TIERS[props.category.tier];
    return {
      label: 'Selected',
      name: props.category.label,
      size: formatBytes(props.category.bytes),
      reason: categoryReason(props.category),
      tier: t,
      action: 'Review',
      path: null,
    };
  }
  return {
    label: 'Nothing selected',
    name: 'Click a segment',
    size: '',
    reason: 'Pick any part of the wheel to see what it is, why it has its tier, and what you can do with it.',
    tier: null,
    action: 'Learn about tiers',
    path: null,
  };
});

const reasons = computed(() => props.item?.candidate?.reasons.map((r) => `${r.tier} · ${r.message}`) ?? []);

function act() {
  const a = view.value.action;
  if (a === 'Add to cleanup' && props.item) emit('add', props.item);
  else if (a === 'Review') void router.push('/cleanup');
  else if (a === 'Why protected?') why.value = !why.value;
  else emit('tiers');
}

const fileManager = /Mac/i.test(globalThis.navigator?.userAgent ?? '')
  ? 'Show in Finder'
  : /Win/i.test(globalThis.navigator?.userAgent ?? '')
    ? 'Show in Explorer'
    : 'Show in folder';
</script>

<template>
  <section aria-label="Selected item" aria-live="polite" class="flex flex-col gap-2.5 rounded-tile-lg bg-surface p-tile-padding-sm" data-testid="selected">
    <div class="flex items-center justify-between gap-2.5">
      <span class="text-label font-medium text-text-tertiary">{{ view.label }}</span>
      <span
        class="rounded-chip border px-2.5 py-[3px] text-caption font-semibold"
        :style="
          view.tier
            ? { background: view.tier.badgeBg, color: view.tier.badgeFg, borderColor: view.tier.badgeBorder }
            : { background: 'var(--color-surface-raised)', color: 'var(--color-text-tertiary)', borderColor: 'var(--color-border)' }
        "
        data-testid="badge"
      >{{ view.tier ? view.tier.label : '—' }}</span>
    </div>
    <div class="flex items-baseline justify-between gap-3">
      <span class="text-[17px] font-semibold">{{ view.name }}</span>
      <span class="font-mono text-[15px]">{{ view.size }}</span>
    </div>
    <span class="text-label text-text-secondary">{{ view.reason }}</span>
    <ul v-if="why" class="m-0 flex flex-col gap-1 pl-4 text-caption text-text-secondary" data-testid="why">
      <li v-for="r in reasons" :key="r">{{ r }}</li>
    </ul>
    <div class="mt-1 flex gap-2">
      <button
        type="button"
        class="min-h-11 cursor-pointer rounded-small border border-border bg-transparent px-3.5 text-label font-medium text-ink"
        :aria-expanded="view.action === 'Why protected?' ? why : undefined"
        data-testid="detail-action"
        @click="act"
      >
        {{ view.action }}
      </button>
      <!-- Revealing a path needs a bridge call that does not exist yet (no shell plugin). -->
      <button
        type="button"
        disabled
        class="min-h-11 cursor-not-allowed rounded-small border-0 bg-transparent px-3 text-label text-text-strong-muted"
        :title="view.path ? `${view.path} · opening folders is not available yet` : 'Opening folders is not available yet'"
      >
        {{ fileManager }}
      </button>
    </div>
  </section>
</template>
