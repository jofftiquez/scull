<script setup lang="ts">
// Projects (P3-UI-06; design 06 / 06b). Projects of the last complete scan from the core's
// `projects()`, with ecosystem filters, tier bars, a detail column and the per-project clean
// with its shrink animation. The clean goes through the Projects action stub
// (components/projects/bridge.ts): the desktop itself never deletes (SAFE-10).
import { computed, onBeforeUnmount, onMounted, reactive, ref } from 'vue';
import { useRoute } from 'vue-router';
import ScreenLayout from '../../components/shell/ScreenLayout.vue';
import ProjectAvatar from '../../components/projects/ProjectAvatar.vue';
import ProjectDetail from '../../components/projects/ProjectDetail.vue';
import TierBar from '../../components/projects/TierBar.vue';
import { designProjects, projectActionsFor } from '../../components/projects/bridge';
import {
  ALL,
  canClean,
  ecosystemLabel,
  filters as filtersOf,
  idleClean,
  lastActivity,
  matches,
  projectSize,
  regenerable,
  shortPath,
  shrink,
  sortBySize,
  tierBar,
} from '../../components/projects/model';
import { toBridgeError, useBridge, type ProjectSummary, type ProjectsView } from '../../bridge';
import { formatBytes } from '../../lib/format';
import { refresh } from '../../state/core';

const bridge = useBridge();
const actions = projectActionsFor(bridge);
const route = useRoute();

const view = ref<ProjectsView | null>(null);
const error = ref<string | null>(null);
const filter = ref(ALL);
const selected = ref<string | null>(null);
const clean = reactive(idleClean());
const cleaned = reactive(new Set<string>());
const toast = ref<{ text: string; bytes: number } | null>(null);
const note = ref<string | null>(null);
const os = ref<string>('macos');
const now = ref(Math.floor(Date.now() / 1000));
let toastTimer: ReturnType<typeof setTimeout> | null = null;

onMounted(async () => {
  try {
    const demo = bridge.kind === 'mock' && route.query.demo === 'design';
    view.value = demo ? designProjects() : await bridge.projects();
    selected.value = sortBySize(view.value.projects)[0]?.root ?? null;
  } catch (e) {
    error.value = toBridgeError(e).message;
  }
  try {
    os.value = (await bridge.appInfo()).os;
  } catch {
    // keep the default label
  }
});
onBeforeUnmount(() => {
  if (toastTimer) clearTimeout(toastTimer);
});

/** A project as currently shown: shrinking while it cleans, without S0 once cleaned. */
function shown(p: ProjectSummary): ProjectSummary {
  if (cleaned.has(p.root)) return shrink(p, 1);
  if (clean.phase === 'cleaning' && clean.root === p.root) return shrink(p, clean.fraction);
  return p;
}

const all = computed(() => view.value?.projects ?? []);
const filterList = computed(() => filtersOf(all.value));
const rows = computed(() => sortBySize(all.value.filter((p) => matches(p, filter.value))).map(shown));
const detail = computed(() => {
  const p = all.value.find((x) => x.root === selected.value);
  return p ? shown(p) : null;
});
const roots = computed(() => (view.value?.roots ?? []).map(shortPath));
const subtitle = computed(() => {
  const n = all.value.length;
  const where = roots.value.length ? ` across ${roots.value.join(' and ')}` : '';
  return `${n} project${n === 1 ? '' : 's'}${where} · ${rows.value.length} shown · found by project markers, never by folder name`;
});
const revealLabel = computed(() => (os.value === 'windows' ? 'Show in Explorer' : os.value === 'linux' ? 'Show in Files' : 'Show in Finder'));

async function cleanSelected() {
  const p = all.value.find((x) => x.root === selected.value);
  if (!p || !canClean(clean, shown(p))) return;
  const bytes = regenerable(p);
  Object.assign(clean, { phase: 'cleaning', root: p.root, fraction: 0, freed: 0, error: null });
  toast.value = null;
  note.value = null;
  try {
    const r = await actions.cleanRegenerable(p.root, bytes, (f) => (clean.fraction = f));
    cleaned.add(p.root);
    Object.assign(clean, { phase: 'cleaned', fraction: 1, freed: r.freed_bytes });
    toast.value = { text: `Cleaned regenerable files in ${p.name}`, bytes: r.freed_bytes };
    if (toastTimer) clearTimeout(toastTimer);
    toastTimer = setTimeout(() => (toast.value = null), 4000);
    if (route.query.demo !== 'design') void refresh(bridge);
  } catch (e) {
    Object.assign(clean, { phase: 'failed', fraction: 0, error: toBridgeError(e).message });
    note.value = toBridgeError(e).message;
  }
}

async function run(action: 'neverSuggest' | 'reveal') {
  if (!detail.value) return;
  try {
    await actions[action](detail.value.root);
    note.value = action === 'neverSuggest' ? `${detail.value.name} won't be suggested again.` : null;
  } catch (e) {
    note.value = toBridgeError(e).message;
  }
}
</script>

<template>
  <ScreenLayout aside-label="Project details">
    <header class="flex flex-wrap items-center justify-between gap-4 rounded-tile bg-surface px-6 py-5">
      <div class="flex flex-col gap-0.5">
        <h1 class="m-0 text-h1 font-semibold tracking-[-0.02em]">Projects</h1>
        <span class="text-label text-text-secondary" data-testid="subtitle">{{ subtitle }}</span>
      </div>
      <div role="group" aria-label="Filter by ecosystem" class="flex flex-wrap gap-1 rounded-chip bg-surface-raised p-1">
        <button
          v-for="f in filterList"
          :key="f.id"
          type="button"
          :aria-pressed="filter === f.id"
          class="min-h-touch min-w-touch cursor-pointer rounded-chip border-0 px-3.5 font-[inherit] text-label font-medium transition-colors duration-200"
          :class="filter === f.id ? 'bg-ink text-on-ink' : 'bg-transparent text-text-strong-muted hover:text-ink'"
          data-testid="filter"
          @click="filter = f.id"
        >
          {{ f.label }}
        </button>
      </div>
    </header>

    <section aria-label="Project list" class="rounded-tile bg-surface p-2">
      <p v-if="error" class="m-0 p-4 text-label text-accent-text" role="alert">{{ error }}</p>
      <p v-else-if="view && all.length === 0" class="m-0 p-6 text-center text-label text-text-secondary">
        No projects yet. Run a scan to find them.
      </p>
      <div v-else class="overflow-x-auto">
        <div class="min-w-[720px]">
          <div class="row-grid px-3.5 py-2.5 text-caption font-medium text-text-tertiary" aria-hidden="true">
            <span></span><span>Project</span><span>Last activity</span><span>By tier</span><span class="text-right">Size</span><span class="text-right">Reclaimable</span>
          </div>
          <ul class="m-0 flex list-none flex-col gap-0.5 p-0">
            <li v-for="p in rows" :key="p.root">
              <button
                type="button"
                class="row-grid min-h-[68px] w-full cursor-pointer rounded-card border-0 px-3.5 py-2.5 text-left font-[inherit] text-ink transition-colors duration-200"
                :class="[selected === p.root ? 'bg-violet-tint' : 'bg-transparent hover:bg-surface-subtle', clean.phase === 'cleaning' && clean.root === p.root ? 'pulse' : '']"
                :aria-pressed="selected === p.root"
                data-testid="project-row"
                @click="selected = p.root"
              >
                <ProjectAvatar :project="p" />
                <span class="flex min-w-0 flex-col gap-[3px]">
                  <span class="font-semibold">{{ p.name }}</span>
                  <span class="truncate font-mono text-caption text-text-tertiary">{{ shortPath(p.root) }} · {{ p.ecosystems.map(ecosystemLabel).join(' · ') }}</span>
                </span>
                <span class="text-label text-text-secondary">{{ lastActivity(p.newest_mtime, now) }}</span>
                <TierBar :segments="tierBar(p)" />
                <span class="text-right font-mono" data-testid="row-size">{{ formatBytes(projectSize(p)) }}</span>
                <span class="text-right font-mono font-medium text-violet-text" data-testid="row-reclaim">
                  {{ regenerable(p) > 0 ? formatBytes(regenerable(p)) : '—' }}
                </span>
              </button>
            </li>
          </ul>
        </div>
      </div>
      <div v-if="view && all.length" class="px-3.5 pt-3 pb-1.5 text-label text-text-tertiary">
        Showing {{ rows.length }} of {{ all.length }} · sorted by size
      </div>
    </section>

    <template v-if="detail" #aside>
      <ProjectDetail
        :project="detail"
        :clean="clean"
        :cleaned="cleaned.has(detail.root)"
        :now="now"
        :toast="toast"
        :note="note"
        :reveal-label="revealLabel"
        @clean="cleanSelected"
        @never-suggest="run('neverSuggest')"
        @reveal="run('reveal')"
      />
    </template>
  </ScreenLayout>
</template>

<style scoped>
.row-grid {
  display: grid;
  grid-template-columns: 44px minmax(0, 1fr) 110px 160px 90px 100px;
  gap: 14px;
  align-items: center;
}
.pulse {
  animation: proj-pulse 800ms ease-in-out infinite;
}
@keyframes proj-pulse {
  0%,
  100% {
    opacity: 1;
  }
  50% {
    opacity: 0.5;
  }
}
</style>
