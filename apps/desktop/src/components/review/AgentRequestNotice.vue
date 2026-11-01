<script setup lang="ts">
// Agent request notification (design 12b), shown by scull itself in the main window when an AI
// agent prepared a plan through MCP: who asked, size, items, plan, time left. It has no Approve
// or Deny button: "Review" opens Cleanup Review in agent request mode, where approving needs the
// full plan on screen, every item reviewed and the typed code. "Later" hides it for this plan
// until the window reloads; the 15-minute clock still runs.
//
// Polls the plan bridge's `approvalRequests` (the core's broker inside Tauri); hidden while the
// request it would show is already open.
import { computed, onBeforeUnmount, onMounted, ref } from 'vue';
import { useRoute, useRouter } from 'vue-router';
import { usePlanBridge, type AgentRequestSummary } from '../../bridge/plan';
import { formatBytes } from '../../lib/format';
import ScullLogo from '../ScullLogo.vue';
import { shortId } from './plan';

const props = withDefaults(defineProps<{ pollMs?: number }>(), { pollMs: 5000 });

const plans = usePlanBridge();
const route = useRoute();
const router = useRouter();
const requests = ref<AgentRequestSummary[]>([]);
const later = ref<Set<string>>(new Set());
const now = ref(Date.now());
let timer: ReturnType<typeof setTimeout> | undefined;
let clock: ReturnType<typeof setInterval> | undefined;
let stopped = false;

async function poll() {
  try {
    const r = await plans.approvalRequests();
    if (!stopped) requests.value = r;
  } catch {
    // Not connected or the database is busy: ask again next time; nothing is shown meanwhile.
  }
  if (!stopped) timer = setTimeout(() => void poll(), props.pollMs);
}

onMounted(() => {
  void poll();
  clock = setInterval(() => (now.value = Date.now()), 15_000);
});
onBeforeUnmount(() => {
  stopped = true;
  clearTimeout(timer);
  clearInterval(clock);
});

const openPlan = computed(() => (route.path === '/cleanup' && typeof route.query.request === 'string' ? route.query.request : null));
const shown = computed(() =>
  requests.value.find((r) => !later.value.has(r.plan_id) && r.plan_id !== openPlan.value && Date.parse(r.expires_at) > now.value) ?? null,
);
const more = computed(() => requests.value.filter((r) => !later.value.has(r.plan_id) && r.plan_id !== openPlan.value).length - 1);
const who = computed(() => shown.value?.client ?? 'An AI agent');
const minutes = computed(() => (shown.value ? Math.max(1, Math.ceil((Date.parse(shown.value.expires_at) - now.value) / 60_000)) : 0));

function review() {
  const r = shown.value;
  if (!r) return;
  later.value = new Set([...later.value, r.plan_id]);
  void router.push({ path: '/cleanup', query: { request: r.plan_id } });
}

function dismiss() {
  const r = shown.value;
  if (r) later.value = new Set([...later.value, r.plan_id]);
}
</script>

<template>
  <section
    v-if="shown"
    :key="shown.plan_id"
    role="region"
    aria-label="Agent request"
    aria-live="polite"
    data-testid="agent-request-notice"
    class="fixed right-6 bottom-6 z-40 flex w-[380px] max-w-[calc(100vw-48px)] gap-3 rounded-tile border border-line bg-surface p-4 text-ink"
    style="animation: scull-rise 360ms cubic-bezier(.2,.9,.3,1.2)"
  >
    <span class="flex h-10 w-10 flex-none items-center justify-center rounded-[12px] bg-surface-raised" aria-hidden="true">
      <ScullLogo :size="24" />
    </span>
    <div class="flex min-w-0 flex-1 flex-col gap-1">
      <div class="flex items-baseline justify-between gap-2 text-caption text-text-secondary">
        <span>scull</span>
        <span v-if="more > 0">+{{ more }} more</span>
      </div>
      <h2 class="m-0 text-body font-semibold">{{ who }} wants to clean {{ formatBytes(shown.reclaimable_bytes) }}</h2>
      <p class="m-0 text-label text-text-strong-muted">
        {{ shown.item_count }} {{ shown.item_count === 1 ? 'item' : 'items' }} · plan {{ shortId(shown.plan_id.replace(/^plan_/, '')) }} ·
        decide within {{ minutes }} min. Nothing runs until you approve in scull.
      </p>
      <div class="mt-2 flex gap-2">
        <button
          type="button"
          class="inline-flex min-h-11 flex-1 cursor-pointer items-center justify-center rounded-button border-0 bg-ink px-4 font-medium text-on-ink"
          data-testid="agent-request-review"
          @click="review"
        >
          Review
        </button>
        <button
          type="button"
          class="inline-flex min-h-11 flex-1 cursor-pointer items-center justify-center rounded-button border border-border bg-surface px-4 font-medium text-ink hover:border-ink"
          data-testid="agent-request-later"
          @click="dismiss"
        >
          Later
        </button>
      </div>
    </div>
  </section>
</template>
