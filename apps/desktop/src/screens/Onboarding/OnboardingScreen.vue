<script setup lang="ts">
import { computed, inject, markRaw, nextTick, onBeforeUnmount, onMounted, reactive, ref, watch } from 'vue';
import { useRouter } from 'vue-router';
import ScullLogo from '../../components/ScullLogo.vue';
import ProjectFolders from '../../components/onboarding/ProjectFolders.vue';
import SetupSummary from '../../components/onboarding/SetupSummary.vue';
import { ONBOARDING_KEY, onboardingBridgeFor } from '../../components/onboarding/bridge';
import { AGENTS, BUILDS, CAUTION, PRIORITIES, canContinue, canMove, checkRoot, defaultAnswers, displayPath, initialScan, scanRoots, summary, totalEntries, type Answers, type Detection, type OnboardingStep } from '../../components/onboarding/model';
import { loadSetup, saveSetup } from '../../components/onboarding/prefs';
import { toBridgeError, useBridge } from '../../bridge';
import { core, refresh } from '../../state/core';

const bridge = useBridge();
const onboarding = inject(ONBOARDING_KEY, () => onboardingBridgeFor(bridge), true);
const router = useRouter();
const saved = loadSetup();
const detection = ref<Detection | null>(null);
const home = ref('');
const contextError = ref<string | null>(null);
const detectionFailed = ref(false);
const answers = reactive<Answers>(saved?.answers ?? { builds: ['other'], roots: [], confirmedBroad: [], agents: 'none', connectAgents: false, priority: 'free-space', caution: 'balanced' });
const touched = new Set<keyof Answers>();
const step = ref<OnboardingStep>(1);
const folderBusy = ref(false);
const removedFolders = ref<string[]>([]);
const heading = ref<HTMLHeadingElement | null>(null);
const saveNote = ref<string | null>(null);
const scan = reactive(initialScan());
let alive = true;
let starting = false;
let controller: AbortController | null = null;
const roots = computed(() => scanRoots(answers, home.value));
const rows = computed(() => summary(answers, { home: home.value, toolchains: [], folders: [], ai_clients: [] }));
const titles = ['What do you build?', 'Where do your projects live?', 'Do you use coding agents?', 'What would you like scull to help with?', 'How should suggestions and reminders behave?', 'Ready for your first scan?'];
const hints = ['Pick all that apply. Your project folders determine what gets scanned.', 'Choose the folders you want scull to look through. Nothing is cleaned during a scan.', 'How often are coding agents part of your work?', 'Choose what brought you here. You can explore everything after your scan.', 'Choose how much explanation you prefer. Cleanup always requires your review.', 'Check your folders and preferences before scull takes a look.'];
const options = computed(() => step.value === 3 ? AGENTS : step.value === 4 ? PRIORITIES : CAUTION);
const choiceKey = computed(() => step.value === 3 ? 'agents' : step.value === 4 ? 'priority' : 'caution');
const ready = computed(() => canContinue(step.value, answers, home.value) && !(step.value === 2 && folderBusy.value));

function applySuggestions() {
  if (!detection.value || !home.value || saved) return;
  const defaults = defaultAnswers({ ...detection.value, home: home.value });
  if (!touched.has('builds')) answers.builds = defaults.builds;
  if (!touched.has('roots')) answers.roots = defaults.roots;
  if (!touched.has('agents')) answers.agents = defaults.agents;
}

async function getContext() {
  contextError.value = null;
  try {
    const context = await onboarding.context();
    if (!alive) return;
    if (!context.home || !checkRoot(context.home, '').ok) throw new Error('Your home folder is unavailable.');
    home.value = context.home;
    if (saved && saved.home !== home.value) {
      answers.roots = [];
      answers.confirmedBroad = [];
    }
    applySuggestions();
  } catch {
    if (alive) contextError.value = 'We could not identify your home folder. Try again before choosing folders.';
  }
}

onMounted(() => {
  void getContext();
  void onboarding.detect().then((d) => {
    if (!alive) return;
    detection.value = d;
    applySuggestions();
  }).catch(() => { if (alive) detectionFailed.value = true; });
});
onBeforeUnmount(() => { alive = false; controller?.abort(); });
watch(() => scan.phase, async () => { await nextTick(); if (alive) heading.value?.focus(); });

async function go(to: OnboardingStep) {
  if (scan.phase === 'running' || folderBusy.value) return;
  step.value = to;
  await nextTick();
  heading.value?.focus();
}

function editFolders() {
  scan.phase = 'idle';
  void go(2);
}

async function startScan(once = false) {
  if (starting || !home.value || !roots.value.length || !canMove(scan.phase, 'running')) return;
  starting = true;
  saveNote.value = null;
  if (!once) {
    const result = saveSetup({ version: 1, home: home.value, answers: { ...answers, connectAgents: false } });
    if (!result.saved) { saveNote.value = result.reason ?? 'Your choices could not be remembered.'; starting = false; return; }
  }
  const selectedRoots = [...roots.value];
  Object.assign(scan, { phase: 'running', entries: 0, activity: 'Starting your scan…', result: null, error: null });
  controller = new AbortController();
  const perRoot: Record<string, number> = {};
  try {
    const report = await bridge.scan({ roots: selectedRoots, allow_broad_roots: selectedRoots.some((r) => answers.confirmedBroad.includes(r)) }, {
      signal: controller.signal,
      onProgress: (p) => {
        if (!alive) return;
        if (p.phase === 'scanning') {
          if (p.root && p.entries != null) perRoot[p.root] = p.entries;
          scan.entries = totalEntries(perRoot);
          scan.activity = p.root ? `${displayPath(p.root, home.value)}/…` : 'Scanning…';
        } else scan.activity = p.what ?? 'Analysing…';
      },
    });
    if (!alive) return;
    scan.result = { entries: report.entries_scanned, projects: report.projects.length, worktrees: report.worktrees.length, safeBytes: report.totals.reclaimable.s0 };
    scan.phase = report.cancelled ? 'cancelled' : 'done';
    if (!report.cancelled || !core.report) core.report = markRaw(report);
    void refresh(bridge);
  } catch (e) {
    if (!alive) return;
    scan.error = toBridgeError(e).message;
    scan.phase = 'failed';
  } finally {
    controller = null;
    starting = false;
  }
}
</script>

<template>
  <main class="onboarding" data-testid="onboarding">
    <header class="setup-header">
      <RouterLink to="/" class="brand" :aria-disabled="scan.phase === 'running'" @click="scan.phase === 'running' && $event.preventDefault()">
        <ScullLogo :size="28" /><span>scull</span>
      </RouterLink>
      <button v-if="scan.phase === 'idle'" type="button" class="quiet-button" data-testid="skip" @click="router.push('/')">Set up later</button>
      <span v-else class="text-label text-text-secondary">Your first scan</span>
    </header>

    <div class="survey-column">
      <template v-if="scan.phase === 'idle'">
        <div class="step-meta" aria-live="polite" data-testid="survey-progress">
          <span>Step {{ step }} of 6</span><span>Getting to know your setup</span>
        </div>
        <div class="step-track" aria-hidden="true"><span v-for="n in 6" :key="n" :class="{ reached: n <= step }"></span></div>
        <section :key="step" class="survey-question" aria-labelledby="survey-heading">
          <h1 id="survey-heading" ref="heading" tabindex="-1">{{ titles[step - 1] }}</h1>
          <p class="question-hint">{{ hints[step - 1] }}</p>
          <fieldset v-if="step === 1" class="build-options">
            <legend class="sr-only">What do you build? Pick all that apply</legend>
            <label v-for="b in BUILDS" :key="b.id" class="build-option" :class="{ selected: answers.builds.includes(b.id) }">
              <input v-model="answers.builds" type="checkbox" :value="b.id" @change="touched.add('builds')" />
              <span>{{ b.label }}</span>
            </label>
          </fieldset>
          <div v-else-if="step === 2">
            <div v-if="contextError" class="context-error" role="alert"><p>{{ contextError }}</p><button type="button" class="outline-button" @click="getContext">Try again</button></div>
            <p v-else-if="!home" class="text-text-secondary" role="status">Preparing folder selection…</p>
            <ProjectFolders v-else v-model:roots="answers.roots" v-model:confirmed="answers.confirmedBroad" v-model:removed="removedFolders" :folders="detection?.folders ?? []" :home="home" :pick-folders="onboarding.pickFolders" @update:roots="touched.add('roots')" @busy="folderBusy = $event" />
          </div>
          <fieldset v-else-if="step < 6" class="choice-options">
            <legend class="sr-only">{{ titles[step - 1] }}</legend>
            <label v-for="option in options" :key="option.id" class="choice-option" :class="{ selected: answers[choiceKey] === option.id }">
              <input v-model="answers[choiceKey]" type="radio" :name="choiceKey" :value="option.id" @change="touched.add(choiceKey)" />
              <span class="choice-copy"><span class="font-medium">{{ option.label }}</span><span v-if="'hint' in option" class="text-label text-text-secondary">{{ option.hint }}</span></span>
            </label>
          </fieldset>
          <SetupSummary v-else :rows="rows" :scan="scan" @edit="go" />
          <p v-if="step === 3" class="preference-note">This records your usage preference. Connecting an agent is a separate action.</p>
          <p v-if="step === 5" class="preference-note">Remembered on this device. Existing cleanup rules and reminder settings stay the same.</p>
          <p v-if="detectionFailed && step === 1" class="preference-note">We couldn't suggest answers from your setup. You can choose everything yourself.</p>
        </section>
        <div v-if="saveNote" class="save-error" role="alert" data-testid="save-note"><p>{{ saveNote }}</p><button type="button" class="outline-button" data-testid="scan-once" @click="startScan(true)">Scan once without saving</button></div>
        <footer class="survey-footer">
          <button v-if="step > 1" type="button" class="quiet-button" data-testid="survey-back" :disabled="folderBusy" @click="go((step - 1) as OnboardingStep)">← Back</button>
          <span v-else class="text-caption text-text-secondary">Your choices stay on this device.</span>
          <button v-if="step < 6" type="button" class="primary-button" data-testid="survey-next" :disabled="!ready" @click="go((step + 1) as OnboardingStep)">Continue →</button>
          <button v-else type="button" class="primary-button" data-testid="start-scan" :disabled="!ready" @click="startScan()">{{ saveNote ? 'Try saving again' : 'Start first scan' }}</button>
        </footer>
      </template>
      <template v-else>
        <h1 ref="heading" tabindex="-1">{{ scan.phase === 'running' ? 'Taking a look through your projects.' : scan.phase === 'done' ? 'Your first scan is ready.' : scan.phase === 'cancelled' ? 'Your scan was stopped.' : 'Let’s try that again.' }}</h1>
        <p class="question-hint">{{ scan.phase === 'running' ? 'scull is reading file metadata. Nothing is being cleaned.' : 'Your files are unchanged. Review what scull found before deciding what to do.' }}</p>
        <SetupSummary :rows="[]" :scan="scan" />
        <footer class="survey-footer">
          <button v-if="scan.phase === 'running'" type="button" class="outline-button" data-testid="cancel-scan" @click="controller?.abort()">Stop scan</button>
          <template v-else>
            <button v-if="scan.phase !== 'done'" type="button" class="quiet-button" data-testid="edit-folders" @click="editFolders">Edit folders</button>
            <button v-if="scan.phase !== 'done'" type="button" class="outline-button" data-testid="retry-scan" @click="startScan()">Try again</button>
            <RouterLink v-if="scan.phase !== 'failed'" to="/" class="primary-button" data-testid="open-overview">{{ scan.phase === 'cancelled' ? 'View partial results' : 'Open your overview' }} →</RouterLink>
          </template>
        </footer>
        <p v-if="saveNote" class="preference-note" data-testid="save-note">{{ saveNote }}</p>
      </template>
    </div>
  </main>
</template>

<style scoped>
.onboarding { flex: 1; min-width: 0; width: 100%; min-height: calc(100vh - 48px); padding: 12px 32px 32px; }
.setup-header { max-width: 1120px; margin: 0 auto; display: flex; justify-content: space-between; align-items: center; min-height: 48px; }
.brand { display: flex; align-items: center; min-height: 44px; gap: 10px; font-size: 20px; font-weight: 650; color: var(--color-ink); text-decoration: none; }
.survey-column { max-width: 640px; margin: 60px auto 0; }
.step-meta { display: flex; justify-content: space-between; gap: 16px; font-size: 13px; color: var(--color-text-secondary); }
.step-meta span:first-child { font-weight: 600; color: var(--color-ink); }
.step-track { display: flex; gap: 6px; margin: 14px 0 42px; }
.step-track span { flex: 1; height: 3px; background: var(--color-border); border-radius: 2px; }
.step-track .reached { background: var(--color-accent); }
.survey-question { min-height: 360px; animation: question-enter 160ms ease-out; }
@keyframes question-enter { from { opacity: .65; transform: translateY(4px); } to { opacity: 1; transform: translateY(0); } }
h1 { margin: 0; font-size: 34px; line-height: 1.18; font-weight: 600; letter-spacing: -.025em; outline: none; }
.question-hint { margin: 16px 0 32px; font-size: 16px; line-height: 1.65; color: var(--color-text-secondary); }
fieldset { border: 0; margin: 0; padding: 0; min-width: 0; }
.build-options { display: grid; grid-template-columns: 1fr 1fr; column-gap: 32px; }
.build-option { display: flex; align-items: center; gap: 12px; min-height: 52px; border-bottom: 1px solid var(--color-line); cursor: pointer; }
input { width: 18px; height: 18px; flex-shrink: 0; margin: 0; accent-color: var(--color-ink); }
.choice-options { display: flex; flex-direction: column; }
.choice-option { display: flex; align-items: center; gap: 16px; padding: 18px 0; min-height: 64px; border-bottom: 1px solid var(--color-line); cursor: pointer; }
.choice-copy { display: flex; flex-direction: column; gap: 5px; }
.selected { color: var(--color-ink); }
.preference-note { margin: 24px 0 0; font-size: 13px; line-height: 1.65; color: var(--color-text-secondary); }
.survey-footer { position: sticky; bottom: 0; background: var(--color-ground); padding-bottom: 12px; display: flex; flex-wrap: wrap; align-items: center; justify-content: space-between; gap: 16px; border-top: 1px solid var(--color-border); padding-top: 24px; margin-top: 36px; }
.primary-button, .outline-button, .quiet-button { display: inline-flex; justify-content: center; align-items: center; min-height: 46px; border-radius: var(--radius-button); font: inherit; font-weight: 500; cursor: pointer; text-decoration: none; }
.primary-button { padding: 0 24px; background: var(--color-ink); color: var(--color-on-ink); border: 1px solid var(--color-ink); margin-left: auto; }
.primary-button:hover { background: var(--color-ink-raised); }
.outline-button { padding: 0 18px; border: 1px solid var(--color-border); background: transparent; color: var(--color-ink); }
.quiet-button { padding: 0 8px; border: 0; background: transparent; color: var(--color-text-secondary); }
.quiet-button:hover, .outline-button:hover { color: var(--color-ink); background: var(--color-surface-raised); }
button:disabled { opacity: .45; cursor: not-allowed; }
button:focus-visible, a:focus-visible, input:focus-visible { outline: 2px solid var(--color-ink); outline-offset: 4px; }
.context-error, .save-error { color: var(--color-accent-text); font-size: 14px; line-height: 1.6; }
@media (max-width: 640px) { .onboarding { padding: 0 4px 24px; } .survey-column { margin-top: 36px; } .step-meta span:last-child { display: none; } .build-options { grid-template-columns: 1fr; } h1 { font-size: 28px; } .survey-question { min-height: 0; } }
@media (prefers-reduced-motion: reduce) { .survey-question { animation: none; } }
</style>
