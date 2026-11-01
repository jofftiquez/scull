<script setup lang="ts">
// Storage Explorer (P3-UI-07; design 07 / 07b). "This Mac" first shows the drives scull scans
// (D-22: external drives when connected; network off by default; backups never), each sized
// by what the last scan found on it; clicking drills into folders with a zoom, breadcrumbs and
// ↑ lead back. Items (candidates, a project's source) are inspected in the right column.
//
// Data: `drives()` and the last scan of this session (core.report); no extra filesystem walk.
// With the mock bridge, `#/explorer?demo=design` shows the design's machine instead
// (components/explorer/designScenario.ts). `?at=Macintosh HD/code` opens a folder directly.
// "Add to cleanup" and "Show in Finder" go through the Explorer action stub
// (components/explorer/actions.ts): the desktop itself never deletes (SAFE-10).
import { computed, onMounted, ref, shallowRef, watch } from 'vue';
import { useRoute, useRouter } from 'vue-router';
import ScreenLayout from '../../components/shell/ScreenLayout.vue';
import Treemap from '../../components/explorer/Treemap.vue';
import FolderDetail from '../../components/explorer/FolderDetail.vue';
import { explorerActionsFor } from '../../components/explorer/actions';
import { designExplorerReport } from '../../components/explorer/designScenario';
import {
  buildExplorer,
  cleanupCandidates,
  detailAction,
  detailKind,
  detailNote,
  displayPath,
  largestInside,
  machineName,
  opens,
  protectionReasons,
  trailOf,
  visibleChildren,
  type ExNode,
} from '../../components/explorer/model';
import { needsOnboarding } from '../../components/onboarding/model';
import { toBridgeError, useBridge, type Drive, type ScanReport } from '../../bridge';
import { formatBytes } from '../../lib/format';
import { core, rescan } from '../../state/core';

const bridge = useBridge();
const actions = explorerActionsFor(bridge);
const route = useRoute();
const router = useRouter();
const demo = bridge.kind === 'mock' && route.query.demo === 'design';

const drives = ref<Drive[] | null>(null);
const drivesError = ref<string | null>(null);
const os = ref('macos');
const designReport = shallowRef<ScanReport | null>(demo ? designExplorerReport() : null);
const path = ref<string[]>([]);
const selected = ref<string | null>(null);
const showAll = ref(false);
const reasons = ref<string[] | null>(null);
const message = ref<string | null>(null);

const report = computed(() => designReport.value ?? core.report);
const explorer = computed(() => (drives.value ? buildExplorer(report.value, drives.value, machineName(os.value)) : null));
const trail = computed(() => (explorer.value ? trailOf(explorer.value.root, path.value) : []));
const here = computed<ExNode | null>(() => trail.value.at(-1) ?? null);
const atRoot = computed(() => trail.value.length <= 1);
const tiles = computed(() => (here.value ? visibleChildren(here.value, showAll.value) : []));
const target = computed<ExNode | null>(() => tiles.value.find((t) => t.id === selected.value) ?? here.value);
const scannedDrives = computed(() => explorer.value?.root.children.length ?? 0);
const subtitle = computed(
  () => `Every drive and development folder by size, colored by risk tier · ${scannedDrives.value} drive${scannedDrives.value === 1 ? '' : 's'} scanned`,
);
const revealLabel = computed(() => (os.value === 'windows' ? 'Show in Explorer' : os.value === 'linux' ? 'Show in Files' : 'Show in Finder'));
const zoomKey = computed(() => path.value.join('\u0000'));

const detail = computed(() => {
  const t = target.value;
  if (!t || !explorer.value || tiles.value.length === 0) return null;
  const isSel = t !== here.value;
  const base = displayPath(trail.value);
  const own = isSel ? `${base}/${t.name}` : base;
  const drives = explorer.value.root.children.length;
  return {
    kind: detailKind(t, isSel),
    name: t.kind === 'machine' ? `${t.name} · ${drives} drive${drives === 1 ? '' : 's'}` : t.name,
    path: isSel && t.kind === 'group' ? base : own,
    bytes: t.bytes,
    tiers: t.tiers,
    largest: largestInside(t, isSel && t.kind !== 'group' ? own : base),
    note: detailNote(t),
    action: detailAction(t),
    canReveal: t.path !== null,
  };
});

watch(
  () => target.value?.id,
  () => {
    reasons.value = null;
    message.value = null;
  },
);

/** Open `?at=Drive/folder/...` (display names; merged tiles like "work/payments" match too). */
function openAt(at: string) {
  const root = explorer.value?.root;
  if (!root) return;
  const ids: string[] = [];
  let node = root;
  let rest = at.replace(/^\/+|\/+$/g, '');
  while (rest) {
    const next = node.children.find((c) => opens(c) && (rest === c.name || rest.startsWith(`${c.name}/`)));
    if (!next) break;
    ids.push(next.id);
    node = next;
    rest = rest.slice(next.name.length).replace(/^\/+/, '');
  }
  path.value = ids;
}

onMounted(async () => {
  const [d, info] = await Promise.allSettled([bridge.drives(), bridge.appInfo()]);
  if (info.status === 'fulfilled') os.value = info.value.os;
  if (d.status === 'fulfilled') drives.value = d.value;
  else {
    drivesError.value = toBridgeError(d.reason).message;
    drives.value = [];
  }
  if (typeof route.query.at === 'string') openAt(route.query.at);
  if (!demo && !core.report && !core.scanning) {
    // First run: scan roots must be chosen explicitly (CFG-04), so onboarding owns the first
    // scan. Never auto-scan default roots before that (same rule as the Overview).
    const status = core.status ?? (await bridge.status().catch(() => null));
    if (!needsOnboarding(status)) void rescan(bridge);
  }
});

watch(
  () => route.query.at,
  (at) => {
    if (typeof at === 'string') openAt(at);
  },
);

function open(n: ExNode) {
  path.value = [...trail.value.slice(1).map((x) => x.id), n.id];
  selected.value = null;
}

function select(n: ExNode) {
  selected.value = selected.value === n.id ? null : n.id;
}

function goTo(index: number) {
  path.value = trail.value.slice(1, index + 1).map((x) => x.id);
  selected.value = null;
}

function up() {
  if (!atRoot.value) goTo(trail.value.length - 2);
}

async function runAction() {
  const t = target.value;
  if (!t || !detail.value) return;
  message.value = null;
  if (detail.value.action === 'why') {
    reasons.value = reasons.value ? null : protectionReasons(t);
  } else if (detail.value.action === 'add') {
    const ids = cleanupCandidates(t).map((c) => c.id);
    try {
      const r = await actions.addToCleanup(ids);
      message.value = `${r.queued} item${r.queued === 1 ? '' : 's'} added to Cleanup Review.`;
    } catch (e) {
      message.value = toBridgeError(e).message;
    }
  } else {
    void router.push('/cleanup');
  }
}

async function reveal() {
  const p = target.value?.path;
  if (!p) return;
  message.value = null;
  try {
    await actions.reveal(p);
  } catch (e) {
    message.value = toBridgeError(e).message;
  }
}
</script>

<template>
  <ScreenLayout aside-label="Selection details">
    <header class="flex flex-wrap items-center justify-between gap-4 rounded-tile bg-surface px-6 py-5">
      <div class="flex flex-col gap-0.5">
        <h1 class="m-0 text-h1 font-semibold tracking-[-0.02em]">Storage Explorer</h1>
        <span class="text-label text-text-secondary" data-testid="subtitle">{{ subtitle }}</span>
      </div>
      <button
        type="button"
        class="hit min-h-10 cursor-pointer rounded-button border border-border px-3.5 font-[inherit] text-label font-medium"
        :class="showAll ? 'bg-ink text-on-ink' : 'bg-surface text-ink'"
        :aria-pressed="showAll"
        title="Show folders too small to draw instead of grouping them"
        data-testid="show-hidden"
        @click="showAll = !showAll"
      >
        Show hidden folders
      </button>
    </header>

    <section aria-label="Folder map" class="flex flex-col gap-3.5 rounded-tile bg-surface p-5">
      <div class="flex flex-wrap items-center gap-1.5">
        <button
          type="button"
          class="hit size-9 cursor-pointer rounded-small border border-border bg-surface font-[inherit] text-ink disabled:cursor-default disabled:text-text-hint"
          :disabled="atRoot"
          aria-label="Up one folder"
          data-testid="up"
          @click="up"
        >
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true" class="mx-auto block">
            <path d="M12 19V5" />
            <path d="M6 11l6-6 6 6" />
          </svg>
        </button>
        <nav aria-label="Breadcrumb" class="flex flex-wrap items-center gap-0.5">
          <template v-for="(c, i) in trail" :key="c.id">
            <button
              type="button"
              class="hit min-h-9 cursor-pointer rounded-small border-0 px-2.5 font-mono text-label text-ink"
              :class="i === trail.length - 1 ? 'bg-surface-raised font-semibold' : 'bg-transparent font-normal'"
              :aria-current="i === trail.length - 1 ? 'page' : undefined"
              data-testid="crumb"
              @click="goTo(i)"
            >
              {{ c.name }}
            </button>
            <span v-if="i < trail.length - 1" aria-hidden="true" class="text-text-hint">›</span>
          </template>
        </nav>
        <span v-if="here" class="ml-auto font-mono text-label text-text-secondary" data-testid="here">
          {{ formatBytes(here.bytes) }} · {{ tiles.length }} item{{ tiles.length === 1 ? '' : 's' }}
        </span>
      </div>

      <div v-if="!explorer" class="grid h-[560px] place-items-center text-label text-text-secondary" aria-busy="true">Reading drives…</div>
      <div v-else-if="tiles.length === 0" class="grid h-[560px] place-items-center rounded-card bg-surface-subtle p-6 text-center" data-testid="empty">
        <div class="flex max-w-md flex-col items-center gap-3">
          <template v-if="core.scanning && !report">
            <span class="text-h3 font-semibold">Scanning…</span>
            <span class="text-label text-text-secondary">The map fills in when the scan finishes.</span>
          </template>
          <template v-else-if="!report">
            <span class="text-h3 font-semibold">Nothing scanned yet</span>
            <span class="text-label text-text-secondary">{{ core.error ?? 'Choose the folders to scan, then the map shows every drive and development folder by size.' }}</span>
            <RouterLink to="/" class="text-label">Go to the Overview</RouterLink>
          </template>
          <template v-else>
            <span class="text-h3 font-semibold">Nothing to show here</span>
            <span class="text-label text-text-secondary">The last scan found nothing in this place.</span>
          </template>
        </div>
      </div>
      <div v-else :key="zoomKey" class="zoom">
        <Treemap :nodes="tiles" :total="here?.bytes ?? 0" :selected="selected" @open="open" @select="select" />
      </div>

      <span class="font-mono text-caption text-text-hint">click a folder to open it · click a file group to inspect · ↑ goes up</span>
      <p v-if="atRoot && report && explorer && explorer.unscanned.length" class="m-0 text-caption text-text-secondary" data-testid="unscanned">
        Not in the map:
        <template v-for="(u, i) in explorer.unscanned" :key="u.drive.mount_point">
          <span :title="u.drive.mount_point">{{ u.drive.label }}</span> · {{ u.why }}<template v-if="i < explorer.unscanned.length - 1">; </template>
        </template>
      </p>
      <p v-if="drivesError" class="m-0 text-caption text-accent-text" role="alert">Drive list unavailable: {{ drivesError }}</p>
    </section>

    <template v-if="detail" #aside>
      <FolderDetail
        v-bind="detail"
        :reveal-label="revealLabel"
        :reasons="reasons"
        :message="message"
        @action="runAction"
        @reveal="reveal"
      />
    </template>
  </ScreenLayout>
</template>

<style scoped>
/* Zoom into / out of a folder (design: scull-zoomA/B, 380ms cubic-bezier(.2,.8,.3,1)).
   Re-keying the map replays it; reduced motion collapses it (styles/main.css). */
.zoom {
  animation: explorer-zoom 380ms cubic-bezier(0.2, 0.8, 0.3, 1);
}
@keyframes explorer-zoom {
  from {
    transform: scale(0.94);
    opacity: 0;
  }
  to {
    transform: none;
    opacity: 1;
  }
}
/* 36px controls (design) with a 44px hit area. */
.hit {
  position: relative;
}
.hit::before {
  content: '';
  position: absolute;
  inset: -4px;
}
@media (prefers-reduced-motion: reduce) {
  .zoom {
    animation: none;
  }
}
</style>
