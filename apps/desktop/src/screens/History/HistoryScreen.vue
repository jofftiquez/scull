<script setup lang="ts">
// History (design 09 / 09b): every recorded operation, what it freed, its audit log, and the
// quarantined items that can still be restored. Data: the read-only `history` bridge command.
// Restore is a mutation, so it goes through the screen-local stub (components/history/bridge.ts):
// the mock animates without touching anything; inside Tauri it refuses until the core restore
// command exists. A restore never overwrites: a taken path restores to a sibling (P2-EXEC-06).
import { computed, onMounted, ref, watch } from 'vue';
import { useRoute } from 'vue-router';
import { toBridgeError, useBridge, type HistoryItem, type HistoryOperation, type HistoryView } from '../../bridge';
import { taskQueue } from '../../state/tasks';
import { formatBytes } from '../../lib/format';
import ScreenLayout from '../../components/shell/ScreenLayout.vue';
import { usePagination } from '../../lib/pagination';
import ListPagination from '../../components/shell/ListPagination.vue';
import ActionFooter from '../../components/shell/ActionFooter.vue';
import ActionBar from '../../components/shell/ActionBar.vue';
import { downloadJson } from '../../components/review/download';
import { historyActionsFor, type HistoryActions, type RestoreResult } from '../../components/history/bridge';
import {
  ICONS,
  LOG_COLORS,
  byLine,
  clock,
  clockSeconds,
  dayLabel,
  displayPath,
  figures,
  freed,
  isRestorable,
  itemState,
  note,
  subLine,
  tag,
  title,
  withRestores,
} from '../../components/history/model';
import '../../components/review/review.css';

const props = defineProps<{ actions?: HistoryActions }>();

const bridge = useBridge();
const route = useRoute();
const actions = props.actions ?? historyActionsFor(bridge);

const raw = ref<HistoryView | null>(null);
const loadError = ref('');
const loading = ref(true);
const picked = ref<string | null>(null);
const restored = ref<Map<string, RestoreResult>>(new Map());
const justRestored = ref<string | null>(null);
const restoring = ref<string | null>(null);
const restoreError = ref('');

onMounted(async () => {
  try {
    // `?demo=empty` (mock only): what the real bridge answers before any cleanup was recorded.
    const empty = bridge.kind === 'mock' && route?.query.demo === 'empty';
    raw.value = empty
      ? { state: 'ok', detail: null, now: Math.floor(Date.now() / 1000), audit_api: true, operations: [] }
      : await bridge.history();

  } catch (e) {
    loadError.value = toBridgeError(e).message;
  } finally {
    loading.value = false;
  }
});

const view = computed(() => (raw.value ? withRestores(raw.value, restored.value) : null));
const now = computed(() => view.value?.now ?? Math.floor(Date.now() / 1000));
const ops = computed(() => view.value?.operations ?? []);
const { page, current: currentPage, rows: pageRows } = usePagination(ops);
watch(page, () => { picked.value = null; });
const current = computed<HistoryOperation | null>(
  () => ops.value.find((o) => o.operation_id === picked.value) ?? null,
);
const fig = computed(() => (view.value ? figures(view.value) : null));

const TAG_CLASS = {
  violet: 'bg-violet-tint text-violet-text',
  accent: 'bg-accent-tint text-accent-text',
  neutral: 'bg-surface-raised text-text-strong-muted',
} as const;
const ITEM_TONE = {
  restored: 'text-violet-text',
  attention: 'text-accent-text',
  muted: 'text-text-tertiary',
} as const;

function pick(op: HistoryOperation) {
  picked.value = op.operation_id;
  restoreError.value = '';
}

/** Arrow keys move through the timeline. */
function onKey(e: KeyboardEvent, i: number) {
  const d = e.key === 'ArrowDown' ? 1 : e.key === 'ArrowUp' ? -1 : 0;
  if (!d) return;
  e.preventDefault();
  const next = pageRows.value[i + d];
  if (!next) return;
  pick(next);
  const el = (e.currentTarget as HTMLElement).closest('ul')?.querySelectorAll<HTMLButtonElement>('button[data-row]')[i + d];
  el?.focus();
}

async function restore(op: HistoryOperation, it: HistoryItem) {
  if (restoring.value || !isRestorable(it, now.value)) return;
  restoring.value = it.item_id;
  restoreError.value = '';
  try {
    const r = await taskQueue.enqueue('Restore item', () => actions.restoreQuarantined(op, it), (r) => ({ summary: `Restored to ${displayPath(r.restored_to)}` }));
    restored.value = new Map([...restored.value, [it.item_id, r]]);
    justRestored.value = it.item_id;
  } catch (e) {
    restoreError.value = `${toBridgeError(e).message}`;
  } finally {
    restoring.value = null;
  }
}

/** The display path split after each separator, so long paths wrap at a folder boundary. */
const pathSegments = (p: string) => displayPath(p).split(/(?<=[\\/])/);

function exportLog() {
  if (!view.value) return;
  downloadJson(`scull-history-${new Date(now.value * 1000).toISOString().slice(0, 10)}.json`, view.value);
}
</script>

<template>
  <ScreenLayout :loading="loading" loading-label="Reading operation history…" aside-label="Operation details" :detail-open="!!current" @close-details="picked = null">
    <header class="flex flex-wrap items-center justify-between gap-3 px-2 py-2">
      <div class="flex flex-col gap-0.5">
        <h1 class="m-0 text-h2 font-semibold">History</h1>
        <span class="text-label text-text-secondary">Every operation, what it freed, and what you can still restore</span>
      </div>
    </header>

    <section v-if="loadError" role="alert" class="rounded-tile border border-accent-tint-border bg-accent-tint p-tile-padding">
      <h2 class="m-0 text-h3 font-semibold">History unavailable</h2>
      <p class="m-0 text-label text-text-strong-muted">{{ loadError }}</p>
    </section>

    <template v-else-if="view && fig">
      <div class="grid grid-cols-2 gap-page-gap xl:grid-cols-4" data-testid="figures">
        <div class="tile flex flex-col gap-0.5 rounded-[20px] bg-surface p-[18px]">
          <span class="text-caption text-text-tertiary">Freed in {{ fig.monthLabel }}</span>
          <span class="text-[30px] font-bold tabular-nums">{{ formatBytes(fig.freedThisMonth) }}</span>
        </div>
        <div class="tile flex flex-col gap-0.5 rounded-[20px] bg-surface p-[18px]">
          <span class="text-caption text-text-tertiary">Operations</span>
          <span class="text-[30px] font-bold tabular-nums">{{ fig.operations }}</span>
        </div>
        <div class="tile flex flex-col gap-0.5 rounded-[20px] bg-surface p-[18px]">
          <span class="text-caption text-text-tertiary">Predicted vs measured</span>
          <span class="text-[30px] font-bold tabular-nums">{{ fig.accuracy == null ? '–' : `${fig.accuracy}%` }}</span>
        </div>
        <div class="tile flex flex-col gap-0.5 rounded-[20px] bg-violet-tint p-[18px]">
          <span class="text-caption text-violet-text">Restorable now</span>
          <span class="text-[30px] font-bold tabular-nums text-violet-text" aria-live="polite" data-testid="restorable">{{ fig.restorable }}</span>
        </div>
      </div>

      <section aria-label="Operations" class="rounded-tile bg-surface p-2">
        <div v-if="!ops.length" class="flex flex-col gap-1 px-4 py-6" data-testid="empty">
          <h2 class="m-0 text-h3 font-semibold">{{ view.state === 'unavailable' ? 'History can’t be read' : 'No cleanups yet' }}</h2>
          <p class="m-0 text-label text-text-secondary">
            <template v-if="view.state === 'unavailable'">{{ view.detail }} Nothing was changed.</template>
            <template v-else-if="!view.audit_api">This build of scull only inspects; cleanups, their audit log and restores appear here once the executor ships.</template>
            <template v-else>Every cleanup you approve appears here with what it freed and what you can restore.</template>
          </p>
        </div>
        <ul class="m-0 flex list-none flex-col gap-0.5 p-0">
          <li v-for="(o, i) in pageRows" :key="o.operation_id">
            <button
              type="button"
              data-row
              :aria-current="current?.operation_id === o.operation_id ? 'true' : undefined"
              class="grid min-h-[68px] w-full cursor-pointer grid-cols-[76px_36px_minmax(0,1fr)_auto] items-center gap-3.5 rounded-card border-0 px-3.5 py-2.5 text-left text-ink transition-[background] duration-200"
              :class="current?.operation_id === o.operation_id ? 'bg-violet-tint' : 'bg-transparent hover:bg-surface-subtle'"
              @click="pick(o)"
              @keydown="onKey($event, i)"
            >
              <span class="flex flex-col">
                <span class="font-semibold">{{ dayLabel(o.started_at, now) }}</span>
                <span class="text-caption text-text-tertiary">{{ clock(o.started_at) }}</span>
              </span>
              <span
                class="flex h-[34px] w-[34px] items-center justify-center rounded-small font-mono text-[11px] font-medium"
                :style="{ background: ICONS[o.kind].bg, color: ICONS[o.kind].fg }"
                :title="ICONS[o.kind].label"
              >{{ ICONS[o.kind].text }}</span>
              <span class="flex min-w-0 flex-col gap-0.5">
                <span class="font-semibold">{{ title(o) }}</span>
                <span class="truncate text-caption text-text-secondary">{{ subLine(o) }}</span>
              </span>
              <span class="flex flex-col items-end gap-1">
                <span class="font-mono font-medium">{{ freed(o) }}</span>
                <span class="rounded-chip px-2 py-0.5 text-[11px] font-semibold" :class="TAG_CLASS[tag(o, now).tone]" data-testid="tag">{{ tag(o, now).text }}</span>
              </span>
            </button>
          </li>
        </ul>
        <ListPagination :page="currentPage" :total="ops.length" @update:page="page = $event" />
      </section>
    </template>

    <ActionFooter aria-label="History actions">
      <ActionBar :summary="`${ops.length} recorded operation${ops.length === 1 ? '' : 's'}`" label="Export log" :disabled="!ops.length" data-testid="export" @action="exportLog" />
    </ActionFooter>

    <template v-if="current" #aside>
      <section aria-live="polite" class="flex flex-col gap-3.5 rounded-tile bg-surface p-tile-padding" data-testid="details">
        <div class="flex flex-col gap-0.5">
          <span class="text-label text-text-tertiary">{{ dayLabel(current.started_at, now) }} · {{ clock(current.started_at) }}</span>
          <h2 class="m-0 text-h2 font-semibold">{{ title(current) }}</h2>
          <span class="text-label text-text-secondary">{{ byLine(current) }}</span>
        </div>
        <div class="grid grid-cols-2 gap-2">
          <div class="flex flex-col rounded-card bg-ground p-3.5">
            <span class="text-caption text-text-tertiary">Predicted</span>
            <span class="text-[24px] font-bold tabular-nums">{{ formatBytes(current.predicted_bytes) }}</span>
          </div>
          <div class="flex flex-col rounded-card bg-violet-tint p-3.5">
            <span class="text-caption text-violet-text">Measured</span>
            <span class="text-[24px] font-bold tabular-nums text-violet-text">{{
              current.measured_delta_bytes == null ? 'not measured' : formatBytes(Math.max(0, current.measured_delta_bytes))
            }}</span>
          </div>
        </div>
        <span v-if="note(current, now)" class="text-label text-text-secondary" data-testid="note">{{ note(current, now) }}</span>

        <div class="flex flex-col">
          <h3 class="m-0 pb-1.5 text-label font-medium text-text-tertiary">Items</h3>
          <ul class="m-0 list-none p-0" data-testid="items">
            <li
              v-for="it in current.items"
              :key="it.item_id"
              class="flex min-h-[58px] items-center gap-2.5 border-t border-line py-2.5"
              :style="justRestored === it.item_id && it.outcome === 'restored' ? 'animation: scull-pop 360ms ease-out' : undefined"
              :data-item="it.item_id"
            >
              <span class="flex min-w-0 flex-1 flex-col">
                <span class="font-mono text-label [overflow-wrap:anywhere]" :title="it.path"><template v-for="(seg, k) in pathSegments(it.path)" :key="k">{{ seg }}<wbr /></template></span>
                <span class="text-caption" :class="ITEM_TONE[restoring === it.item_id ? 'attention' : itemState(current, it).tone]" data-testid="item-state">{{
                  restoring === it.item_id ? 'restoring…' : itemState(current, it).text
                }}</span>
              </span>
              <span class="font-mono text-label">{{ formatBytes(it.moved_bytes ?? it.freed_bytes ?? it.predicted_bytes) }}</span>
              <button
                v-if="isRestorable(it, now)"
                type="button"
                class="min-h-[44px] min-w-[88px] cursor-pointer rounded-small border border-border bg-surface px-3 text-caption font-semibold text-ink hover:border-ink disabled:cursor-progress"
                :disabled="!!restoring"
                :aria-label="`Restore ${displayPath(it.path)}`"
                :style="restoring === it.item_id ? 'animation: scull-pulse 600ms ease-in-out infinite' : undefined"
                data-testid="restore"
                @click="restore(current, it)"
              >
                {{ restoring === it.item_id ? 'Restoring…' : 'Restore' }}
              </button>
            </li>
          </ul>
          <span v-if="restoreError" role="alert" class="pt-2 text-label text-accent-text" data-testid="restore-error">{{ restoreError }}</span>
        </div>

        <div class="flex flex-col gap-1.5">
          <div class="flex items-baseline justify-between gap-2">
            <h3 class="m-0 text-label font-medium text-text-tertiary">Audit log</h3>
            <span class="font-mono text-[11px]" :class="current.chain.intact ? 'text-text-tertiary' : 'font-medium text-accent-text'" data-testid="chain">
              {{ current.chain.intact ? `hash chain verified · ${current.chain.rows} rows` : `chain damaged: ${current.chain.problems.join(', ')}` }}
            </span>
          </div>
          <div class="flex flex-col rounded-[14px] bg-ink p-3.5 font-mono text-caption leading-[1.7] text-on-ink-soft" data-testid="log">
            <span v-for="(l, i) in current.log" :key="i">
              <span class="text-on-ink-subtle">{{ clockSeconds(l.at) }}</span>&nbsp; <span :style="{ color: LOG_COLORS[l.kind] }">{{ l.text }}</span>
            </span>
          </div>
        </div>
      </section>
    </template>
  </ScreenLayout>
</template>
