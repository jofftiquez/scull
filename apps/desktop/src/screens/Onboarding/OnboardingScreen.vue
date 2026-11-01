<script setup lang="ts">
// First-run onboarding (P3-UI-03; PRD §14.7 ONB-01…07; design 02 / 02b). Detection first, then
// five pre-filled questions, a live summary, and an animated first scan of the chosen roots.
// Full window, no sidebar (route meta `bare`). Nothing here deletes anything: the scan is the
// core's read-only scan, and the profile goes through the onboarding bridge stub (see
// components/onboarding/bridge.ts).
import { computed, onBeforeUnmount, onMounted, reactive, ref } from 'vue';
import { useRouter } from 'vue-router';
import ScullLogo from '../../components/ScullLogo.vue';
import ChoiceCard from '../../components/onboarding/ChoiceCard.vue';
import FoundOnMachine from '../../components/onboarding/FoundOnMachine.vue';
import ProjectFolders from '../../components/onboarding/ProjectFolders.vue';
import QuestionTile from '../../components/onboarding/QuestionTile.vue';
import SetupSummary from '../../components/onboarding/SetupSummary.vue';
import { onboardingBridgeFor } from '../../components/onboarding/bridge';
import {
  AGENTS,
  BUILDS,
  CAUTION,
  PRIORITIES,
  aiClientLabel,
  andList,
  answeredCount,
  canMove,
  defaultAnswers,
  displayPath,
  initialScan,
  machineNoun,
  scanFill,
  scanRoots,
  summary,
  toProfile,
  totalEntries,
  type Answers,
  type BuildId,
  type Detection,
} from '../../components/onboarding/model';
import { toBridgeError, useBridge, type Drive } from '../../bridge';
import { refresh } from '../../state/core';

const bridge = useBridge();
const onboarding = onboardingBridgeFor(bridge);
const router = useRouter();

const detection = ref<Detection | null>(null);
const drive = ref<Drive | null>(null);
const os = ref<string | undefined>(undefined);
const answers = reactive<Answers>({
  builds: [],
  roots: [],
  confirmedBroad: [],
  agents: 'none',
  connectAgents: false,
  priority: 'free-space',
  caution: 'balanced',
});
const scan = reactive(initialScan());
const elapsed = ref(0);
const saveNote = ref<string | null>(null);
let timer: ReturnType<typeof setInterval> | null = null;
let controller: AbortController | null = null;

const noun = computed(() => machineNoun(os.value));
const home = computed(() => detection.value?.home ?? '');
const rows = computed(() => (detection.value ? summary(answers, detection.value) : []));
const answered = computed(() => answeredCount(answers, home.value));
const roots = computed(() => scanRoots(answers, home.value));
const fill = computed(() => scanFill(scan.phase, elapsed.value));
const clients = computed(() => (detection.value?.ai_clients ?? []).map(aiClientLabel));

onMounted(async () => {
  const [det, drives, info] = await Promise.allSettled([onboarding.detect(), bridge.drives(), bridge.appInfo()]);
  const d: Detection = det.status === 'fulfilled' ? det.value : { home: '', toolchains: [], folders: [], ai_clients: [] };
  detection.value = d;
  Object.assign(answers, defaultAnswers(d));
  if (drives.status === 'fulfilled') drive.value = drives.value.find((x) => x.holds_home) ?? drives.value[0] ?? null;
  if (info.status === 'fulfilled') os.value = info.value.os;
});

onBeforeUnmount(() => {
  if (timer) clearInterval(timer);
  controller?.abort();
});

function toggleBuild(id: BuildId) {
  answers.builds = answers.builds.includes(id)
    ? answers.builds.filter((b) => b !== id)
    : BUILDS.map((b) => b.id).filter((b) => b === id || answers.builds.includes(b));
}

const reducedMotion = () => globalThis.matchMedia?.('(prefers-reduced-motion: reduce)').matches ?? false;
const wait = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

async function save(): Promise<void> {
  if (!detection.value) return;
  try {
    const r = await onboarding.saveProfile(toProfile(answers, detection.value));
    saveNote.value = r.saved ? null : (r.reason ?? 'The profile was not saved.');
  } catch (e) {
    saveNote.value = `The profile was not saved: ${toBridgeError(e).message}`;
  }
}

async function skip() {
  await save();
  await router.push('/');
}

async function startScan() {
  if (!detection.value || roots.value.length === 0 || !canMove(scan.phase, 'running')) return;
  await save();
  const scanRootsNow = roots.value;
  Object.assign(scan, { phase: 'running', entries: 0, activity: '', result: null, error: null });
  elapsed.value = 0;
  const t0 = Date.now();
  timer = setInterval(() => (elapsed.value = Date.now() - t0), 40);
  controller = new AbortController();
  const perRoot: Record<string, number> = {};
  try {
    const report = await bridge.scan(
      { roots: scanRootsNow, allow_broad_roots: scanRootsNow.some((r) => answers.confirmedBroad.includes(r)) },
      {
        signal: controller.signal,
        onProgress: (p) => {
          if (p.phase === 'scanning') {
            if (p.root && p.entries != null) perRoot[p.root] = p.entries;
            scan.entries = totalEntries(perRoot);
            scan.activity = p.root ? `${displayPath(p.root, home.value)}/…` : 'scanning…';
          } else {
            scan.activity = p.what ?? 'analysing…';
          }
        },
      },
    );
    // Let the bar finish its ease so a fast scan doesn't flash past (design: 3.2 s).
    if (!reducedMotion() && !report.cancelled) await wait(Math.max(0, 1600 - (Date.now() - t0)));
    scan.result = {
      entries: report.entries_scanned,
      projects: report.projects.length,
      worktrees: report.worktrees.length,
      safeBytes: report.totals.reclaimable.s0,
    };
    scan.phase = report.cancelled ? 'cancelled' : 'done';
    void refresh(bridge);
  } catch (e) {
    scan.error = toBridgeError(e).message;
    scan.phase = 'failed';
  } finally {
    if (timer) clearInterval(timer);
    timer = null;
    controller = null;
  }
}

function cancelScan() {
  controller?.abort();
}
</script>

<template>
  <main class="w-full" data-testid="onboarding">
    <div class="mx-auto grid max-w-[1200px] grid-cols-12 gap-page-gap">
      <header class="col-span-12 flex flex-wrap items-end justify-between gap-6 rounded-tile bg-surface p-8">
        <div class="flex flex-col gap-4">
          <div class="flex items-center gap-3">
            <ScullLogo :size="30" :wink="scan.phase === 'done'" />
            <span class="text-h2 font-semibold">scull</span>
          </div>
          <h1 class="m-0 text-[40px] leading-[1.1] font-semibold tracking-[-0.03em]">Let's set scull up for how you work.</h1>
          <p class="m-0 text-[16px] text-text-secondary">
            Five quick questions, under a minute. We already looked around, so most answers are pre-filled. Everything
            stays on this {{ noun }}.
          </p>
        </div>
        <span class="font-mono text-caption text-text-secondary" data-testid="answered">{{ answered }} of 5 answered</span>
      </header>

      <FoundOnMachine class="tile col-span-12 row-span-2 min-[861px]:col-span-4" :detection="detection" :drive="drive" :noun="noun" />

      <QuestionTile n="01" title="What do you build?" hint="Pick all that apply" gap="md" class="tile col-span-12 min-[861px]:col-span-8">
        <div class="flex flex-wrap gap-2">
          <button
            v-for="b in BUILDS"
            :key="b.id"
            type="button"
            :aria-pressed="answers.builds.includes(b.id)"
            class="min-h-touch cursor-pointer rounded-chip border-0 px-4 font-[inherit] font-medium"
            :class="answers.builds.includes(b.id) ? 'bg-ink text-on-ink' : 'bg-surface-raised text-ink hover:bg-line'"
            @click="toggleBuild(b.id)"
          >
            {{ b.label }}
          </button>
        </div>
        <span class="text-caption text-text-secondary">Pre-selected from what we found. Decides which categories show first.</span>
      </QuestionTile>

      <QuestionTile n="02" title="Where do your projects live?" class="tile col-span-12 min-[861px]:col-span-4">
        <ProjectFolders
          v-if="detection"
          v-model:roots="answers.roots"
          v-model:confirmed="answers.confirmedBroad"
          :folders="detection.folders"
          :home="detection.home"
        />
      </QuestionTile>

      <QuestionTile n="03" title="Do you use AI coding agents?" class="tile col-span-12 min-[861px]:col-span-4">
        <div class="grid grid-cols-3 gap-1.5">
          <button
            v-for="o in AGENTS"
            :key="o.id"
            type="button"
            :aria-pressed="answers.agents === o.id"
            class="min-h-touch cursor-pointer rounded-button border-0 font-[inherit] font-medium"
            :class="answers.agents === o.id ? 'bg-ink text-on-ink' : 'bg-surface-raised text-ink hover:bg-line'"
            @click="answers.agents = o.id"
          >
            {{ o.label }}
          </button>
        </div>
        <span class="text-label text-text-secondary">
          {{ clients.length ? `Found ${andList(clients)}.` : 'No AI coding clients found.' }}
        </span>
        <label v-if="clients.length && answers.agents !== 'none'" class="flex min-h-touch cursor-pointer items-center gap-3">
          <input v-model="answers.connectAgents" type="checkbox" class="m-0 size-[18px] accent-ink" />
          <span>Connect scull to them, read-only</span>
        </label>
      </QuestionTile>

      <QuestionTile n="04" title="What matters most right now?" class="tile col-span-12 min-[861px]:col-span-6">
        <div class="flex flex-col gap-1.5">
          <ChoiceCard
            v-for="o in PRIORITIES"
            :key="o.id"
            :label="o.label"
            :hint="o.hint"
            :pressed="answers.priority === o.id"
            @pick="answers.priority = o.id"
          />
        </div>
      </QuestionTile>

      <QuestionTile n="05" title="How cautious should scull be?" class="tile col-span-12 min-[861px]:col-span-6">
        <div class="flex flex-col gap-1.5">
          <ChoiceCard
            v-for="o in CAUTION"
            :key="o.id"
            :label="o.label"
            :hint="o.hint"
            :pressed="answers.caution === o.id"
            @pick="answers.caution = o.id"
          />
        </div>
        <span class="text-caption text-text-secondary">
          Safety rules are identical for every choice. This only changes suggestions and how often scull alerts you.
        </span>
      </QuestionTile>

      <SetupSummary class="col-span-12" :rows="rows" :scan="scan" :fill="fill">
        <template v-if="scan.phase === 'idle'">
          <button type="button" class="btn-ghost" data-testid="skip" @click="skip">Skip, use defaults</button>
          <button
            type="button"
            class="btn-accent"
            data-testid="start-scan"
            :disabled="roots.length === 0"
            :title="roots.length === 0 ? 'Choose at least one folder' : undefined"
            @click="startScan"
          >
            Start first scan
          </button>
        </template>
        <button v-else-if="scan.phase === 'running'" type="button" class="btn-ghost" data-testid="cancel-scan" @click="cancelScan">
          Stop scan
        </button>
        <template v-else-if="scan.phase === 'failed'">
          <button type="button" class="btn-ghost" @click="skip">Skip for now</button>
          <button type="button" class="btn-accent" @click="startScan">Try again</button>
        </template>
        <RouterLink v-else to="/" class="btn-open" data-testid="open-overview">Open your overview →</RouterLink>
        <p v-if="saveNote" class="m-0 basis-full text-caption text-text-strong-muted" data-testid="save-note">{{ saveNote }}</p>
      </SetupSummary>
    </div>
  </main>
</template>

<style scoped>
.btn-ghost,
.btn-accent {
  min-height: 48px;
  border-radius: var(--radius-button);
  font: inherit;
  cursor: pointer;
}
.btn-ghost {
  padding: 0 20px;
  background: transparent;
  color: var(--color-ink);
  border: 1px solid var(--color-text-strong-muted);
  font-weight: 500;
}
.btn-ghost:hover {
  background: var(--color-surface);
}
.btn-accent {
  padding: 0 24px;
  border: 0;
  background: var(--color-tier-s0);
  color: var(--color-tier-s0-text);
  font-weight: 600;
}
.btn-accent:disabled {
  cursor: not-allowed;
  opacity: 0.55;
}
.btn-open {
  min-height: 52px;
  padding: 0 28px;
  display: inline-flex;
  align-items: center;
  border-radius: var(--radius-button);
  background: var(--color-tier-s0);
  color: var(--color-tier-s0-text);
  text-decoration: none;
  font-weight: 700;
  font-size: 16px;
  animation: scull-bounce 620ms cubic-bezier(0.2, 0.9, 0.3, 1.3) both;
}
.btn-open:hover {
  color: var(--color-tier-s0-text);
}
@keyframes scull-bounce {
  0% {
    transform: scale(0.4);
    opacity: 0;
  }
  55% {
    transform: scale(1.12);
    opacity: 1;
  }
  75% {
    transform: scale(0.96);
  }
  100% {
    transform: scale(1);
  }
}
</style>
