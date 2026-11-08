<script setup lang="ts">
import LoadingState from '../../components/shell/LoadingState.vue';
import ActionFooter from '../../components/shell/ActionFooter.vue';
// Cleanup Review in "agent request" mode (design 12, PRD §13.3, D-7, D-34 provisional).
//
// An AI tool call is not consent: the human reviews every item, types the confirmation code
// shown only here (the agent never sees it) and approves this exact plan once. States:
// pending → ready → approved → running → executed, or expired / denied / revoked / changed.
// The state machine is ./approval.ts (pure, unit-tested); this component feeds it clicks,
// bridge answers, broker events and a 1 s clock.
import { computed, onBeforeUnmount, onMounted, ref, shallowRef } from 'vue';
import { RouterLink } from 'vue-router';
import { toBridgeError } from '../../bridge';
import { usePlanBridge, type ApprovalEvent, type ApprovalRequest } from '../../bridge/plan';
import { formatBytes } from '../../lib/format';
import PlanRow from '../../components/review/PlanRow.vue';
import SafetyChecks from '../../components/review/SafetyChecks.vue';
import StatTile from '../../components/review/StatTile.vue';
import SuccessMark from '../../components/review/SuccessMark.vue';
import TierBadge from '../../components/review/TierBadge.vue';
import {
  allReviewed,
  approvalReducer,
  codeStatus,
  initialApproval,
  isClosed,
  reviewedCount,
  secondsLeft,
  viewOf,
  type ApprovalAction,
  type ApprovalState,
} from '../../components/review/approval';
import { clock, displayPath, itemTitle, mmss, planTotals, shortId, undoLabel } from '../../components/review/plan';
import ReviewHeader from './ReviewHeader.vue';
import { downloadJson } from '../../components/review/download';
import '../../components/review/review.css';
import { usePagination } from '../../lib/pagination';
import ListPagination from '../../components/shell/ListPagination.vue';

const props = defineProps<{ planId: string }>();
const plans = usePlanBridge();

const req = shallowRef<ApprovalRequest | null>(null);
const st = ref<ApprovalState | null>(null);
const loadError = ref('');
const actionError = ref('');
const busy = ref(false);
const showTerminal = ref(false);
let tick: ReturnType<typeof setInterval> | null = null;
let unwatch: (() => void) | null = null;

function dispatch(a: ApprovalAction) {
  if (st.value) st.value = approvalReducer(st.value, a);
}

function onEvent(e: ApprovalEvent) {
  if (e.kind === 'approved') dispatch({ type: 'approved_elsewhere', at: Date.parse(e.at), channel: e.channel });
  else if (e.kind === 'denied') dispatch({ type: 'deny', at: Date.parse(e.at) });
  else if (e.kind === 'revoked') dispatch({ type: 'revoke', at: Date.parse(e.at) });
  else if (e.kind === 'expired') dispatch({ type: 'tick', now: Math.max(Date.parse(e.at), st.value?.expiresAt ?? 0) });
  else if (e.kind === 'agent_started') dispatch({ type: 'agent_started', at: Date.parse(e.at) });
  else if (e.kind === 'item_started') dispatch({ type: 'item_started', id: e.item_id });
  else if (e.kind === 'item_finished') dispatch({ type: 'item_finished', outcome: e.outcome });
  else if (e.kind === 'finished') dispatch({ type: 'finished', result: e.result, at: Date.parse(e.result.finished_at) });
  else dispatch({ type: 'invalidated', id: e.item_id, reason: e.reason, at: Date.parse(e.at) });
}

onMounted(async () => {
  try {
    const r = await plans.approvalRequest(props.planId);
    if (!r) {
      loadError.value = `There is no agent request ${props.planId}. It may have been handled already.`;
      return;
    }
    req.value = r;
    st.value = initialApproval({
      itemIds: r.plan.items.map((i) => i.candidate.id),
      code: r.code,
      expiresAt: Date.parse(r.plan.expires_at),
      now: Date.now(),
    });
    unwatch = plans.watchApproval(props.planId, onEvent);
    tick = setInterval(() => {
      if (st.value && (st.value.phase === 'pending' || st.value.phase === 'approved')) dispatch({ type: 'tick', now: Date.now() });
    }, 1000);
  } catch (e) {
    loadError.value = toBridgeError(e).message;
  }
});
onBeforeUnmount(() => {
  if (tick) clearInterval(tick);
  unwatch?.();
});

const plan = computed(() => req.value?.plan ?? null);
const src = computed(() => (plan.value?.source.kind === 'mcp' ? plan.value.source : null));
/** Who asked, mid-sentence ("the agent" when the plan doesn't record the client) ... */
const client = computed(() => src.value?.client ?? 'the agent');
/** ... and at the start of a sentence. */
const Client = computed(() => client.value.charAt(0).toUpperCase() + client.value.slice(1));
/** "by you" here, or the other human channel it was approved on (e.g. your terminal). */
const approvedBy = computed(() => (st.value?.approvedVia === 'terminal' ? 'by you in your terminal' : 'by you'));
const items = computed(() => plan.value?.items ?? []);
const totals = computed(() => planTotals(items.value, new Set(items.value.map((i) => i.candidate.id))));
const n = computed(() => items.value.length);
const phase = computed(() => st.value?.phase ?? 'pending');
const view = computed(() => (st.value ? viewOf(st.value) : 'pending'));
const pending = computed(() => phase.value === 'pending');
const execView = computed(() => phase.value === 'running' || phase.value === 'executed');
const closed = computed(() => (st.value ? isClosed(st.value) : false));
const left = computed(() => (st.value ? secondsLeft(st.value) : 0));
const totalSecs = computed(() => (plan.value ? Math.round((Date.parse(plan.value.expires_at) - Date.parse(plan.value.created_at)) / 1000) : 900));
const pid = computed(() => (plan.value ? shortId(plan.value.plan_id) : ''));
const at = (t: number | null | undefined) => (t ? clock(t) : '');
const expiresClock = computed(() => (plan.value ? clock(plan.value.expires_at) : ''));
const itemsWord = computed(() => `${n.value} ${n.value === 1 ? 'item' : 'items'}`);
const doneCount = computed(() => Object.keys(st.value?.outcomes ?? {}).length);

const subline = computed(() => {
  const s = st.value;
  const p = `plan ${pid.value}`;
  if (!s || !plan.value) return 'loading the request…';
  switch (s.phase) {
    case 'approved':
      return `${p} · approved ${approvedBy.value} at ${at(s.approvedAt)} · waiting for ${client.value} · single use`;
    case 'running':
      return `${p} · ${client.value} is running it · approved ${approvedBy.value} at ${at(s.approvedAt)}`;
    case 'executed':
      return `${p} · done at ${at(s.closedAt)} · approval spent`;
    case 'expired':
      return `${p} · expired at ${at(s.closedAt)} · nothing changed`;
    case 'denied':
      return `${p} · denied at ${at(s.closedAt)} · nothing changed`;
    case 'revoked':
      return `${p} · approval revoked at ${at(s.closedAt)} · nothing changed`;
    case 'changed':
      return `${p} · invalidated at ${at(s.closedAt)} · nothing changed`;
    default:
      return `${p} · prepared by ${client.value} ${clock(plan.value.created_at)} · single use · nothing runs until you approve`;
  }
});

const askTitle = computed(() => {
  const what = `${itemsWord.value} · ${formatBytes(totals.value.bytes)}`;
  if (phase.value === 'executed') return `${Client.value} cleaned ${itemsWord.value} with your approval`;
  return closed.value ? `${Client.value} asked to clean ${what}` : `${Client.value} wants to clean ${what}`;
});

/** The right-hand tile: countdown, then Ready / Spent / closed. */
const timer = computed(() => {
  const s = st.value;
  const base = {
    tone: 'normal' as 'normal' | 'urgent' | 'done' | 'closed',
    label: 'Time left to decide',
    big: mmss(left.value),
    sub: `Expires at ${expiresClock.value}. After that the plan is void.`,
    dash: Math.round((left.value / totalSecs.value) * 214),
    check: false,
  };
  if (!s) return base;
  if (s.phase === 'pending' && left.value <= 60) return { ...base, tone: 'urgent' as const, sub: 'Expiring now. Approve or deny.' };
  if (s.phase === 'approved' || s.phase === 'running') {
    return { ...base, tone: 'done' as const, label: `Approved at ${at(s.approvedAt)}`, big: 'Ready', sub: `Single use · lapses at ${expiresClock.value} if unused`, dash: 214, check: true };
  }
  if (s.phase === 'executed') {
    return { ...base, tone: 'done' as const, label: 'Approval', big: 'Spent', sub: `Used once at ${at(s.startedAt)} · can't run again`, dash: 214, check: true };
  }
  if (s.phase === 'expired') return { ...base, tone: 'closed' as const, label: `Expired at ${at(s.closedAt)}`, big: '0:00', sub: 'Nothing ran. The plan is void.', dash: 0 };
  if (s.phase === 'denied') return { ...base, tone: 'closed' as const, label: `Denied at ${at(s.closedAt)}`, big: 'Denied', sub: 'Nothing ran. The plan is closed.', dash: 0 };
  if (s.phase === 'revoked') return { ...base, tone: 'closed' as const, label: `Revoked at ${at(s.closedAt)}`, big: 'Revoked', sub: 'Nothing ran. The plan is closed.', dash: 0 };
  if (s.phase === 'changed') return { ...base, tone: 'closed' as const, label: `Void since ${at(s.closedAt)}`, big: 'Void', sub: 'A target changed on disk.', dash: 0 };
  return base;
});

/** Visual fingerprint of the hash (design 12): a mirrored 5×5 grid in three inks. */
const fingerprint = computed(() => {
  const hex = (plan.value?.hash ?? '').padEnd(16, '0');
  const pal = ['var(--color-surface-raised)', 'var(--color-ink)', 'var(--color-accent)'];
  const cells: { x: number; y: number; fill: string }[] = [];
  for (let r = 0; r < 5; r++) {
    for (let c = 0; c < 3; c++) {
      const fill = pal[parseInt(hex[(r * 3 + c) % 16]!, 16) % 3]!;
      cells.push({ x: c * 12, y: r * 12, fill });
      if (c < 2) cells.push({ x: (4 - c) * 12, y: r * 12, fill });
    }
  }
  const h = hex.slice(0, 16);
  const tag =
    phase.value === 'changed'
      ? { text: 'Void', cls: 'bg-accent-tint text-accent-text', sub: 'The disk no longer matches this fingerprint.' }
      : closed.value
        ? {
            text: 'Closed',
            cls: 'bg-surface-raised text-text-strong-muted',
            sub: 'This fingerprint can never be approved again.',
          }
        : { text: 'Matches', cls: 'bg-violet-tint text-violet-text', sub: `Same as the plan ID ${client.value} showed you. Any change voids approval.` };
  return { cells, lines: [`${h.slice(0, 4)} ${h.slice(4, 8)}`, `${h.slice(8, 12)} ${h.slice(12, 16)}`], tag };
});

const table = computed(() => {
  const s = st.value;
  const k = s ? reviewedCount(s) : 0;
  switch (phase.value) {
    case 'approved':
      return { title: 'Approved plan', hint: `Locked: approval is bound to these ${itemsWord.value}` };
    case 'running':
      return { title: `${Client.value} is running this plan`, hint: `${doneCount.value} of ${n.value} done · each item re-checked first` };
    case 'executed': {
      const skipped = Object.values(s?.outcomes ?? {}).filter((o) => o.outcome === 'skipped').length;
      return { title: 'Result', hint: `${n.value - skipped} of ${n.value} done · ${skipped} skipped · measured, not estimated` };
    }
    case 'expired':
      return { title: 'Plan (expired)', hint: 'Read-only · nothing was changed' };
    case 'denied':
    case 'revoked':
      return { title: `Plan (${phase.value})`, hint: 'Read-only · nothing was changed' };
    case 'changed':
      return { title: 'Plan (no longer valid)', hint: '1 item changed after the plan was made' };
    default:
      return { title: 'Review each item', hint: `${k} of ${n.value} reviewed · tick each one once you have looked at it` };
  }
});

function itemStatus(id: string): 'done' | 'active' | 'todo' | 'skipped' {
  const s = st.value;
  if (!s) return 'todo';
  const o = s.outcomes[id];
  if (o) return o.outcome === 'skipped' ? 'skipped' : 'done';
  return s.active === id ? 'active' : 'todo';
}

function outcomeText(id: string): string {
  const o = st.value?.outcomes[id];
  if (!o) return '';
  if (o.outcome === 'skipped') return `Skipped: ${o.reason ?? 'changed'}`;
  if (o.outcome === 'quarantined') return 'Moved · 7-day undo';
  if (o.outcome === 'trashed') return 'Moved to Trash';
  return `Freed ${formatBytes(o.freed_bytes)}`;
}

function trail(id: string, undo: string): string {
  if (phase.value === 'changed' && st.value?.changed?.id === id) return 'Changed';
  if (phase.value === 'executed') return outcomeText(id);
  if (phase.value === 'running') {
    const k = itemStatus(id);
    return k === 'done' || k === 'skipped' ? outcomeText(id) : k === 'active' ? 'Re-checking…' : 'Waiting';
  }
  return undo;
}

const code = computed(() => codeStatus(st.value ?? initialApproval({ itemIds: [], code: '', expiresAt: 0, now: 0 })));
const ready = computed(() => view.value === 'ready');
const remaining = computed(() => (st.value ? n.value - reviewedCount(st.value) : n.value));
const codeSpaced = computed(() => (req.value?.code ?? '').split('').join(' ').replace(/ - /g, ', dash, '));

async function approve() {
  const s = st.value;
  const p = plan.value;
  if (!s || !p || busy.value) return;
  if (!ready.value) {
    dispatch({ type: 'approve', at: Date.now() }); // not ready: shakes the code field
    return;
  }
  busy.value = true;
  actionError.value = '';
  try {
    const r = await plans.approvePlan({ plan_id: p.plan_id, hash: p.hash, code: s.code });
    dispatch({ type: 'approve', at: Date.parse(r.approved_at) });
  } catch (e) {
    const err = toBridgeError(e);
    if (err.kind === 'plan_expired') dispatch({ type: 'tick', now: Date.parse(p.expires_at) });
    actionError.value = err.message;
  } finally {
    busy.value = false;
  }
}

async function deny() {
  const p = plan.value;
  if (!p || !pending.value || busy.value) return;
  busy.value = true;
  try {
    const r = await plans.denyPlan({ plan_id: p.plan_id });
    dispatch({ type: 'deny', at: Date.parse(r.denied_at) });
  } catch (e) {
    actionError.value = toBridgeError(e).message;
  } finally {
    busy.value = false;
  }
}

async function revoke() {
  const p = plan.value;
  if (!p || phase.value !== 'approved' || busy.value) return;
  busy.value = true;
  try {
    const r = await plans.revokeApproval({ plan_id: p.plan_id });
    dispatch({ type: 'revoke', at: Date.parse(r.revoked_at) });
  } catch (e) {
    actionError.value = toBridgeError(e).message;
  } finally {
    busy.value = false;
  }
}

const steps = computed(() => {
  const s = st.value;
  const running = phase.value === 'running';
  return [
    { text: `Plan prepared by ${client.value}`, time: plan.value ? clock(plan.value.created_at) : '', state: 'done' },
    { text: s?.approvedVia === 'terminal' ? 'Approved by you in your terminal' : 'Approved by you in scull', time: at(s?.approvedAt), state: 'done' },
    {
      text: running ? `${Client.value} is running it · ${doneCount.value} of ${n.value}` : `Waiting for ${client.value} to run it`,
      time: running ? at(s?.startedAt) : '',
      state: 'active',
    },
    { text: 'Measured result, logged in History', time: '', state: 'todo' },
  ];
});

const closedInfo = computed(() => {
  const s = st.value;
  const p = pid.value;
  if (!s) return null;
  switch (s.phase) {
    case 'expired':
      return {
        title: 'This request expired · nothing changed',
        text: `No one approved plan ${p} in time, so it is void. ${Client.value} gets "expired" when it checks and can prepare a new plan if you still want one.`,
        log: `audit · ${at(s.closedAt)} · plan ${p} · expired · 0 items touched`,
        icon: '0:00', iconCls: 'bg-surface-raised text-text-strong-muted text-[14px]',
      };
    case 'denied':
      return {
        title: 'You denied this request · nothing changed',
        // A human's denial is final (#78): no channel can approve the plan, and the agent's
        // status check says `denied`.
        text: `This plan can never be approved now. ${Client.value} is told it was denied the next time it checks; a new request needs a new plan and a new review.`,
        log: `audit · ${at(s.closedAt)} · plan ${p} · denied via desktop · 0 items touched`,
        icon: '✕', iconCls: 'bg-ink text-on-ink text-[20px]',
      };
    case 'revoked':
      return {
        title: 'Approval revoked · nothing changed',
        text: `You took the approval back before ${client.value} used it. The plan is closed and can't be approved again.`,
        log: `audit · ${at(s.closedAt)} · plan ${p} · revoked via desktop · 0 items touched`,
        icon: '✕', iconCls: 'bg-ink text-on-ink text-[20px]',
      };
    case 'changed': {
      const it = items.value.find((i) => i.candidate.id === s.changed?.id);
      return {
        title: 'This plan changed since it was prepared',
        text: `${it ? itemTitle(it) : 'A target'} was modified after ${client.value} made the plan, so the plan no longer matches the disk and can't be approved. Nothing was changed. ${Client.value} is told the plan was invalidated and can prepare a fresh one.`,
        log: `audit · ${at(s.closedAt)} · plan ${p} · invalidated (target changed) · 0 items touched`,
        icon: '≠', iconCls: 'bg-tier-s2 text-tier-s2-text text-[22px]',
      };
    }
    default:
      return null;
  }
});

const footnote = computed(
  () => `Items that fail are skipped and reported to you and to ${client.value}. They are never cleaned "best effort".`,
);

const measured = computed(() => st.value?.result?.measured_freed_bytes ?? 0);
const predictedFreed = computed(() => totals.value.deleteBytes);

const btn = 'inline-flex h-11 cursor-pointer items-center rounded-button border border-border bg-surface px-5 font-medium text-ink no-underline hover:border-ink';
const { page, current: currentPage, rows: pageItems } = usePagination(items);
</script>

<template>
  <ReviewHeader :subline="subline" agent :pulse="pending">
    <button type="button" :class="btn" :disabled="!plan" @click="plan && downloadJson(`scull-plan-${plan.plan_id}.json`, plan)">Export plan</button>
    <button type="button" :class="btn" :disabled="!plan" :aria-expanded="showTerminal" @click="showTerminal = !showTerminal">Approve in terminal</button>
  </ReviewHeader>
  <p v-if="showTerminal && plan" role="status" class="m-0 rounded-tile bg-surface px-6 py-3 text-label">
    Prefer a terminal? Run <code class="font-mono">scull approve {{ plan.plan_id }}</code> in your own terminal. It shows the same
    plan and asks for a code of its own.
  </p>

  <LoadingState v-if="!req && !loadError" label="Loading agent request…" />
  <section v-if="loadError" role="alert" class="rounded-tile border border-accent-tint-border bg-accent-tint p-tile-padding">
    <h2 class="m-0 text-h3 font-semibold">No request to review</h2>
    <p class="m-0 text-label text-text-strong-muted">{{ loadError }}</p>
  </section>

  <div v-if="plan && st" class="grid grid-cols-12 gap-page-gap" :data-state="view">
    <!-- Executed (12f) -->
    <section
      v-if="phase === 'executed' && st.result"
      aria-label="Cleanup complete"
      aria-live="polite"
      class="tile relative col-span-12 flex flex-wrap items-center gap-7 overflow-hidden rounded-tile bg-surface p-8"
      style="animation: scull-rise 320ms ease-out"
    >
      <SuccessMark burst />
      <div class="flex flex-[1_1_320px] flex-col gap-1">
        <span class="text-[48px] leading-none font-bold tracking-[-0.03em]" style="animation: scull-bounce 640ms 200ms cubic-bezier(.2,.9,.3,1.3) both">
          {{ formatBytes(measured) }} freed
        </span>
        <span class="text-[16px]" style="animation: scull-rise 420ms 700ms ease-out both">
          Measured <strong>{{ formatBytes(measured) }}</strong> of {{ formatBytes(predictedFreed) }} predicted
          <template v-if="predictedFreed">({{ Math.round((measured / predictedFreed) * 100) }}%)</template>
          <template v-if="st.result.moved_bytes"> · <strong>{{ formatBytes(st.result.moved_bytes) }}</strong> moved to quarantine</template>
          · {{ itemsWord }}
        </span>
        <span class="text-label text-text-secondary" style="animation: scull-rise 420ms 900ms ease-out both">
          Run by {{ client }} at {{ at(st.startedAt) }} with your approval from {{ at(st.approvedAt) }}. The approval is spent; anything else
          needs a new plan and a new approval.
        </span>
      </div>
      <div class="flex flex-wrap gap-2" style="animation: scull-rise 420ms 1000ms ease-out both">
        <RouterLink to="/cleanup" class="inline-flex min-h-12 items-center rounded-button border border-border bg-surface px-5 font-medium text-ink no-underline">
          Dismiss
        </RouterLink>
        <RouterLink to="/history" class="inline-flex min-h-12 items-center rounded-button bg-ink px-[22px] font-semibold text-on-ink no-underline hover:text-on-ink">
          Open in History
        </RouterLink>
      </div>
    </section>

    <!-- Who is asking -->
    <section aria-label="Who is asking" class="tile col-span-12 flex flex-col gap-3.5 rounded-tile bg-surface p-tile-padding lg:col-span-8">
      <div class="flex items-center gap-3.5">
        <span aria-hidden="true" class="flex h-12 w-12 flex-none items-center justify-center rounded-[14px] bg-surface-raised font-mono text-body font-medium">
          {{ src?.initials ?? 'AI' }}
        </span>
        <div class="flex min-w-0 flex-col gap-0.5">
          <h2 class="m-0 text-h2 font-semibold tracking-[-0.01em]">{{ askTitle }}</h2>
          <span v-if="src && src.pid != null && src.command && src.cwd" class="text-label text-text-secondary" data-testid="asker">
            Asked through MCP <span class="font-mono">{{ src.tool }}</span> at {{ clock(plan.created_at) }} · name reported by the client ·
            scull mcp serve, pid {{ src.pid }}, started by <span class="font-mono">{{ src.command }}</span> in {{ displayPath(src.cwd) }}
          </span>
          <span v-else-if="src && src.client" class="text-label text-text-secondary" data-testid="asker">
            Asked through MCP <span class="font-mono">{{ src.tool }}</span> at {{ clock(plan.created_at) }} · name reported by the client,
            not verified · through scull mcp serve on this computer, for your user
          </span>
          <span v-else-if="src" class="text-label text-text-secondary" data-testid="asker">
            Asked through MCP <span class="font-mono">{{ src.tool }}</span> at {{ clock(plan.created_at) }} · prepared by an AI agent
            through scull mcp serve on this computer, for your user · the client didn't name itself
          </span>
        </div>
      </div>
      <div v-if="src?.note" class="flex flex-col gap-1 rounded-card border border-line bg-surface-subtle px-4 py-3.5">
        <span class="text-caption font-semibold text-text-secondary">Agent's note · written by the agent, shown as-is</span>
        <span class="font-mono text-label [overflow-wrap:anywhere]">"{{ src.note }}"</span>
      </div>
      <span class="text-label text-text-strong-muted">
        {{
          pending
            ? `An AI tool call is not your consent. Only you can approve this, here or in your own terminal. ${client} never sees the code below.`
            : 'An AI tool call is not your consent. Every request, approval, denial and result is kept in History with the plan fingerprint.'
        }}
      </span>
    </section>

    <!-- Time left / Ready / Spent / closed -->
    <section
      aria-label="Time left to decide"
      class="tile col-span-12 flex items-center gap-[18px] rounded-tile border p-tile-padding lg:col-span-4"
      :class="timer.tone === 'urgent' ? 'border-accent-tint-border bg-accent-tint' : 'border-surface bg-surface'"
    >
      <svg width="88" height="88" viewBox="0 0 88 88" aria-hidden="true" class="flex-none">
        <circle
          cx="44" cy="44" r="34" fill="none" stroke-width="8"
          :stroke="timer.tone === 'done' ? 'var(--color-violet-tint)' : timer.tone === 'closed' ? 'var(--color-surface-raised)' : 'var(--color-accent-track)'"
        />
        <circle
          cx="44" cy="44" r="34" fill="none" stroke-width="8" stroke-linecap="round" transform="rotate(-90 44 44)"
          :stroke="timer.tone === 'done' ? 'var(--color-tier-s0)' : 'var(--color-accent)'"
          :stroke-dasharray="`${timer.dash} 214`"
          :style="timer.tone === 'closed' ? 'opacity: 0' : 'transition: stroke-dasharray 1s linear'"
          data-testid="timer-ring"
        />
        <path
          v-if="timer.check"
          d="M30 45 l9 9 l19 -20" fill="none" stroke="var(--color-ink)" stroke-width="6" stroke-linecap="round" stroke-linejoin="round"
          stroke-dasharray="48" stroke-dashoffset="48" style="animation: scull-check 380ms 200ms ease-out forwards"
        />
      </svg>
      <div role="timer" aria-live="off" :aria-label="pending ? `${mmss(left)} left to decide` : timer.big" class="flex min-w-0 flex-col gap-0.5">
        <span class="text-label font-medium text-text-secondary">{{ timer.label }}</span>
        <span
          class="text-stat-md leading-[1.1] font-semibold tracking-[-0.03em] tabular-nums"
          :class="timer.tone === 'urgent' ? 'text-accent-text' : timer.tone === 'closed' && phase === 'expired' ? 'text-text-secondary' : 'text-ink'"
          :style="timer.tone === 'urgent' ? 'animation: scull-pulse 1000ms ease-in-out infinite' : undefined"
          data-testid="timer-big"
        >{{ timer.big }}</span>
        <span class="text-label text-text-secondary">{{ timer.sub }}</span>
      </div>
    </section>

    <StatTile label="In this plan" :value="formatBytes(totals.bytes)" :sub="`${itemsWord} · predicted reclaimable`" />
    <StatTile label="Delete · S0" :value="formatBytes(totals.deleteBytes)" sub="Freed now · not undoable · rebuilt by your tools" tone="accent" />
    <StatTile label="Quarantine · S1/S2" :value="formatBytes(totals.quarantineBytes)" sub="7-day undo · freed when purged" />
    <section aria-label="Plan fingerprint" class="tile col-span-12 flex flex-col gap-2 rounded-tile bg-surface p-tile-padding-sm sm:col-span-6 xl:col-span-3">
      <div class="flex items-center justify-between gap-2">
        <span class="text-label font-medium text-text-secondary">Plan fingerprint</span>
        <span class="rounded-chip px-2 py-0.5 text-[11px] font-semibold" :class="fingerprint.tag.cls" data-testid="fp-tag">{{ fingerprint.tag.text }}</span>
      </div>
      <div class="flex items-center gap-3" :style="{ opacity: phase === 'changed' ? 0.45 : closed ? 0.6 : 1 }">
        <svg width="58" height="58" viewBox="0 0 58 58" role="img" aria-label="Visual fingerprint of the plan hash" class="flex-none">
          <rect v-for="(c, i) in fingerprint.cells" :key="i" :x="c.x" :y="c.y" width="10" height="10" rx="2" :fill="c.fill" />
        </svg>
        <span class="font-mono text-[15px] leading-normal font-medium" :class="{ 'line-through': phase === 'changed' }">
          {{ fingerprint.lines[0] }}<br />{{ fingerprint.lines[1] }}
        </span>
      </div>
      <span class="text-caption text-text-secondary">{{ fingerprint.tag.sub }}</span>
    </section>

    <!-- Items -->
    <section aria-label="Plan items" class="tile col-span-12 overflow-hidden rounded-tile bg-surface pt-2">
      <div class="flex flex-wrap items-center justify-between gap-2 px-6 pt-2.5 pb-1.5">
        <h2 class="m-0 text-[15px] font-semibold">{{ table.title }}</h2>
        <span aria-live="polite" class="text-label text-text-secondary">{{ table.hint }}</span>
      </div>
      <div class="overflow-x-auto">
        <div role="list" class="min-w-[1000px]">
          <div class="agent-grid py-2 text-label font-medium text-text-secondary" aria-hidden="true">
            <span class="pl-6">{{ execView ? 'Status' : 'Reviewed' }}</span><span>Item</span><span>Tier</span><span>Evidence</span><span>Action</span>
            <span class="pr-4 text-right">Predicted</span><span>{{ phase === 'executed' ? 'Outcome' : phase === 'running' ? 'Now' : 'Undo' }}</span><span></span>
          </div>
          <PlanRow
            v-for="it in pageItems"
            :key="it.candidate.id"
            :item="it"
            grid="agent-grid"
            :dim="phase === 'expired' || phase === 'denied' || phase === 'revoked'"
            :sub="phase === 'changed' && st.changed?.id === it.candidate.id ? st.changed.reason : undefined"
            :row-class="{
              'row-changed': phase === 'changed' && st.changed?.id === it.candidate.id,
              'row-active': itemStatus(it.candidate.id) === 'active',
              'bg-surface-subtle': pending && !st.reviewed[it.candidate.id],
            }"
          >
            <template #lead>
              <label v-if="!execView" class="flex h-16 cursor-pointer items-center gap-2 pl-6 text-caption text-text-strong-muted" :class="{ 'cursor-default': !pending }">
                <input
                  type="checkbox"
                  class="m-0 h-5 w-5 accent-ink"
                  :checked="!!st.reviewed[it.candidate.id]"
                  :disabled="!pending"
                  :aria-label="`Reviewed ${itemTitle(it)}`"
                  @change="dispatch({ type: 'review', id: it.candidate.id, on: ($event.target as HTMLInputElement).checked })"
                />
                <span aria-hidden="true">{{ st.reviewed[it.candidate.id] ? 'Seen' : 'Review' }}</span>
              </label>
              <span
                v-else
                role="img"
                :aria-label="itemStatus(it.candidate.id) === 'done' ? 'Done' : itemStatus(it.candidate.id) === 'active' ? 'Re-checking and cleaning' : itemStatus(it.candidate.id) === 'skipped' ? 'Skipped' : 'Waiting'"
                class="flex h-16 items-center pl-6"
              >
                <span
                  class="flex h-6 w-6 items-center justify-center rounded-chip border-2"
                  :class="
                    itemStatus(it.candidate.id) === 'done'
                      ? 'border-tier-s0 bg-tier-s0'
                      : itemStatus(it.candidate.id) === 'active'
                        ? 'border-accent bg-surface'
                        : 'border-border bg-surface'
                  "
                  :style="
                    itemStatus(it.candidate.id) === 'active'
                      ? 'animation: scull-pulse 800ms ease-in-out infinite'
                      : itemStatus(it.candidate.id) === 'done'
                        ? 'animation: scull-pop 340ms ease-out'
                        : undefined
                  "
                >
                  <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="var(--color-on-fill)" stroke-width="4" stroke-linecap="round" stroke-linejoin="round" :style="{ opacity: itemStatus(it.candidate.id) === 'done' ? 1 : 0 }">
                    <path d="M5 12l5 5 9-10" />
                  </svg>
                </span>
              </span>
            </template>
            <template #trail>
              <span :class="phase === 'running' || phase === 'executed' || phase === 'changed' ? 'text-ink' : ''">{{ trail(it.candidate.id, undoLabel(it.candidate)) }}</span>
            </template>
          </PlanRow>
          <template v-if="plan.left_out.length">
            <div role="listitem"><h3 class="m-0 border-t border-line px-6 pt-4 pb-2 text-label font-medium text-text-secondary">Left out by scull · not in this plan, can't be added</h3></div>
            <div
              v-for="l in plan.left_out"
              :key="l.label"
              role="listitem"
              class="grid min-h-14 grid-cols-[104px_minmax(280px,2.4fr)_132px_minmax(320px,3fr)] items-center border-t border-line text-text-secondary"
            >
              <span></span>
              <span class="font-mono text-label">{{ displayPath(l.label) }}</span>
              <span><TierBadge :tier="l.tier" /></span>
              <span class="pr-6 text-label">{{ l.reason }}</span>
            </div>
          </template>
        </div>
      </div>
      <ListPagination :page="currentPage" :total="items.length" @update:page="page = $event" />
    </section>

    <section aria-label="What approving does" class="tile col-span-12 flex flex-col gap-2.5 rounded-tile bg-surface p-tile-padding lg:col-span-6">
      <h2 class="m-0 text-label font-medium text-text-secondary">What approving does</h2>
      <ul class="m-0 flex list-none flex-col gap-2 p-0 text-label">
        <li class="flex gap-2.5">
          <span aria-hidden="true" class="flex h-[22px] w-[22px] flex-none items-center justify-center rounded-[7px] bg-violet-tint font-mono text-[11px] text-violet-text">1×</span>
          <span>Lets {{ client }} run <strong>this exact plan once</strong>. After one run the approval is spent.</span>
        </li>
        <li class="flex gap-2.5">
          <span aria-hidden="true" class="flex h-[22px] w-[22px] flex-none items-center justify-center rounded-[7px] bg-violet-tint font-mono text-[11px] text-violet-text">#</span>
          <span>Bound to fingerprint <span class="font-mono">{{ shortId(plan.hash) }}</span>. If anything in the plan changes, the approval no longer matches and nothing runs.</span>
        </li>
        <li class="flex gap-2.5">
          <span aria-hidden="true" class="flex h-[22px] w-[22px] flex-none items-center justify-center rounded-[7px] bg-violet-tint font-mono text-[11px] text-violet-text">{{ Math.round(totalSecs / 60) }}</span>
          <span>Lapses at {{ expiresClock }}, {{ Math.round(totalSecs / 60) }} minutes after the plan was made, if {{ client }} hasn't used it.</span>
        </li>
        <li class="flex gap-2.5">
          <span aria-hidden="true" class="flex h-[22px] w-[22px] flex-none items-center justify-center rounded-[7px] bg-violet-tint font-mono text-[11px] text-violet-text">✕</span>
          <span>The agent gets no token or code. It can only ask whether you approved, so it can't approve its own plan.</span>
        </li>
      </ul>
    </section>
    <SafetyChecks :footnote="footnote" />

    <!-- Pending / ready (12, 12c) -->
    <ActionFooter
      v-if="pending"
      aria-label="Approve or deny"
      class="tile col-span-12 flex flex-col gap-[18px] rounded-tile border border-accent-tint-border bg-accent-tint p-tile-padding text-ink"
    >
      <div class="flex flex-wrap items-center justify-between gap-4">
        <div class="flex flex-col gap-1">
          <h2 class="m-0 text-h3 font-semibold">Approve {{ itemsWord }} · {{ formatBytes(totals.bytes) }} for {{ client }}</h2>
          <span class="text-label text-text-strong-muted">
            Deleted S0 items can't be undone; rebuild them with your tools. Quarantined items can be restored for 7 days.
          </span>
          <span v-if="actionError" role="alert" class="text-label text-accent-text">{{ actionError }}</span>
        </div>
        <div class="flex flex-wrap gap-2">
          <button
            type="button"
            class="min-h-12 cursor-pointer rounded-button border border-text-strong-muted bg-transparent px-[22px] font-medium text-ink"
            :disabled="busy"
            data-testid="deny"
            @click="deny"
          >
            Deny
          </button>
          <button
            type="button"
            class="min-h-12 min-w-[220px] rounded-button border-0 px-6 font-semibold"
            :class="ready ? 'cursor-pointer bg-ink text-on-ink' : 'cursor-not-allowed bg-surface-raised text-text-secondary'"
            :aria-disabled="!ready"
            data-testid="approve"
            @click="approve"
          >
            Approve {{ itemsWord }}
          </button>
        </div>
      </div>
      <div class="flex flex-wrap items-end gap-4 border-t border-accent-tint-border pt-[18px]">
        <div class="flex flex-col gap-1.5">
          <span id="code-hint" class="text-label font-medium text-text-strong-muted">Type this code to approve</span>
          <span
            :aria-label="`Confirmation code: ${codeSpaced}`"
            class="inline-flex min-h-[52px] items-center rounded-button border border-accent-tint-border bg-surface px-[18px] font-mono text-[26px] font-medium tracking-[0.18em]"
            data-testid="code"
          >{{ req?.code }}</span>
        </div>
        <label class="flex flex-col gap-1.5">
          <span class="text-label font-medium text-text-strong-muted">Confirmation code</span>
          <input
            type="text"
            :value="st.code"
            autocomplete="off"
            autocapitalize="characters"
            spellcheck="false"
            maxlength="9"
            aria-describedby="code-hint approve-status"
            placeholder="Type the code"
            class="h-[52px] w-[220px] rounded-button border-2 bg-surface px-4 font-mono text-[22px] tracking-[0.12em] text-ink outline-none placeholder:font-sans placeholder:text-[16px] placeholder:tracking-normal focus-visible:outline-2 focus-visible:outline-accent"
            :class="code === 'match' ? 'border-tier-s0' : code === 'mismatch' || st.shake ? 'border-accent-text' : 'border-border'"
            :style="st.shake ? 'animation: scull-shake 360ms ease-in-out' : undefined"
            data-testid="code-input"
            @input="dispatch({ type: 'code', value: ($event.target as HTMLInputElement).value })"
            @keydown.enter="approve"
          />
        </label>
        <div id="approve-status" aria-live="polite" class="flex flex-[1_1_260px] flex-col gap-1.5 pb-1 text-label">
          <span class="flex items-center gap-2" :class="allReviewed(st) ? 'text-ink' : 'text-text-strong-muted'">
            <span aria-hidden="true" class="inline-flex h-[18px] w-[18px] items-center justify-center rounded-chip text-[11px] font-bold" :class="allReviewed(st) ? 'bg-tier-s0 text-tier-s0-text' : 'bg-text-hint text-on-ink'">
              {{ allReviewed(st) ? '✓' : remaining }}
            </span>
            {{ allReviewed(st) ? `All ${n} items reviewed` : `Review ${remaining} more item${remaining === 1 ? '' : 's'} in the list above` }}
          </span>
          <span
            class="flex items-center gap-2"
            :class="code === 'match' ? 'text-ink' : code === 'mismatch' ? 'text-accent-text' : 'text-text-strong-muted'"
          >
            <span
              aria-hidden="true"
              class="inline-flex h-[18px] w-[18px] items-center justify-center rounded-chip text-[11px] font-bold"
              :class="code === 'match' ? 'bg-tier-s0 text-tier-s0-text' : code === 'mismatch' ? 'bg-accent-text text-on-ink' : 'bg-text-hint text-on-ink'"
            >{{ code === 'match' ? '✓' : code === 'mismatch' ? '!' : code === 'typing' ? '…' : '·' }}</span>
            {{
              code === 'match'
                ? 'Code matches'
                : code === 'mismatch'
                  ? "That code doesn't match. Check each character."
                  : code === 'typing'
                    ? `Keep typing: ${st.code.toUpperCase().replace(/[^A-Z0-9]/g, '').length} of ${(req?.code ?? '').replace(/[^A-Za-z0-9]/g, '').length} characters`
                    : 'Type the code shown on the left'
            }}
          </span>
        </div>
      </div>
    </ActionFooter>

    <!-- Approved / running (12d, 12e) -->
    <ActionFooter
      v-if="phase === 'approved' || phase === 'running'"
      aria-label="Approved"
      aria-live="polite"
      class="tile col-span-12 flex flex-wrap items-center gap-6 rounded-tile border border-tier-s1 bg-surface p-tile-padding"
      style="animation: scull-rise 320ms ease-out"
    >
      <SuccessMark :size="64" track="var(--color-violet-tint)" label="Approved" />
      <div class="flex flex-[1_1_300px] flex-col gap-1">
        <h2 class="m-0 text-[22px] font-semibold tracking-[-0.01em]">{{ phase === 'approved' ? 'Approved for one run' : 'Running with your approval' }}</h2>
        <span class="text-label text-text-strong-muted">
          {{
            phase === 'approved'
              ? `${Client} can now run plan ${pid} once. It hasn't yet: nothing has been deleted. You can still revoke until it starts.`
              : 'Each item is re-checked right before it is touched. Anything that changed is skipped and reported.'
          }}
        </span>
        <span v-if="actionError" role="alert" class="text-label text-accent-text">{{ actionError }}</span>
      </div>
      <ol aria-label="Progress" class="m-0 flex flex-[1_1_420px] list-none flex-col gap-2 p-0">
        <li v-for="s in steps" :key="s.text" class="grid grid-cols-[22px_minmax(0,1fr)_auto] items-center gap-2.5 text-label" :class="s.state === 'todo' ? 'text-text-secondary' : 'text-ink'">
          <span
            aria-hidden="true"
            class="inline-flex h-[18px] w-[18px] items-center justify-center rounded-chip border-2 text-[10px] font-bold text-tier-s0-text"
            :class="s.state === 'done' ? 'border-tier-s0 bg-tier-s0' : s.state === 'active' ? 'border-accent bg-surface' : 'border-border bg-surface'"
            :style="s.state === 'active' ? 'animation: scull-pulse 1000ms ease-in-out infinite' : undefined"
          >{{ s.state === 'done' ? '✓' : '' }}</span>
          <span><span class="sr-only">{{ s.state === 'done' ? 'Done: ' : s.state === 'active' ? 'In progress: ' : 'Next: ' }}</span>{{ s.text }}</span>
          <span class="font-mono text-caption text-text-secondary">{{ s.time }}</span>
        </li>
      </ol>
      <button
        v-if="phase === 'approved'"
        type="button"
        class="min-h-12 cursor-pointer rounded-button border border-text-strong-muted bg-surface px-5 font-medium text-ink"
        :disabled="busy"
        data-testid="revoke"
        @click="revoke"
      >
        Revoke approval
      </button>
    </ActionFooter>

    <!-- Closed: expired / denied / revoked / changed (12g, 12h, 12i) -->
    <ActionFooter
      v-if="closedInfo"
      :aria-label="closedInfo.title"
      role="status"
      class="tile col-span-12 flex flex-wrap items-center justify-between gap-4 rounded-tile border border-line bg-surface p-tile-padding"
      style="animation: scull-rise 320ms ease-out"
    >
      <div class="flex flex-[1_1_520px] items-center gap-4">
        <span aria-hidden="true" class="flex h-12 w-12 flex-none items-center justify-center rounded-[14px] font-mono font-medium" :class="closedInfo.iconCls">{{ closedInfo.icon }}</span>
        <div class="flex flex-col gap-1">
          <h2 class="m-0 text-h3 font-semibold">{{ closedInfo.title }}</h2>
          <span class="text-label text-text-strong-muted">{{ closedInfo.text }}</span>
          <span class="font-mono text-caption text-text-secondary">{{ closedInfo.log }}</span>
        </div>
      </div>
      <div class="flex flex-wrap gap-2">
        <RouterLink to="/history" class="inline-flex min-h-12 items-center rounded-button border border-border bg-surface px-5 font-medium text-ink no-underline">
          Open in History
        </RouterLink>
        <RouterLink to="/cleanup" class="inline-flex min-h-12 items-center rounded-button bg-ink px-[22px] font-semibold text-on-ink no-underline hover:text-on-ink">
          Dismiss
        </RouterLink>
      </div>
    </ActionFooter>
  </div>
</template>
