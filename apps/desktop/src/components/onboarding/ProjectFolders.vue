<script setup lang="ts">
// "Where do your projects live?" (design 02, ONB-02 #2). Detected folders with checkboxes and
// "Add folder". CFG-04: a filesystem root, the home folder or a folder containing it is never
// ticked by default, and ticking one asks for explicit confirmation first.
import { computed, nextTick, ref } from 'vue';
import { broadRootConcern, checkRoot, displayPath, type DetectedFolder } from './model';

const props = defineProps<{ folders: DetectedFolder[]; home: string }>();
const roots = defineModel<string[]>('roots', { required: true });
const confirmed = defineModel<string[]>('confirmed', { required: true });

const added = ref<DetectedFolder[]>([]);
const list = computed(() => [...props.folders, ...added.value]);
const pending = ref<string | null>(null);
const adding = ref(false);
const typed = ref('');
const error = ref<string | null>(null);
const input = ref<HTMLInputElement | null>(null);
const confirmBtn = ref<HTMLButtonElement | null>(null);

const concern = (p: string) => broadRootConcern(p, props.home);

/** Tick or untick, keeping roots in the order the folders are listed. */
function setRoot(path: string, on: boolean) {
  const set = new Set(roots.value.filter((r) => r !== path));
  if (on) set.add(path);
  const ordered = list.value.map((f) => f.path).filter((p) => set.delete(p));
  roots.value = [...ordered, ...set];
}

function tick(path: string, on: boolean) {
  if (on && concern(path) !== null && !confirmed.value.includes(path)) {
    pending.value = path;
    void nextTick(() => confirmBtn.value?.focus());
    return;
  }
  setRoot(path, on);
  if (!on) confirmed.value = confirmed.value.filter((r) => r !== path);
}

function confirmBroad(yes: boolean) {
  const p = pending.value;
  pending.value = null;
  if (!p || !yes) return;
  confirmed.value = [...confirmed.value.filter((r) => r !== p), p];
  setRoot(p, true);
}

function openAdd() {
  adding.value = true;
  error.value = null;
  void nextTick(() => input.value?.focus());
}

function add() {
  const c = checkRoot(typed.value, props.home);
  if (!c.ok) {
    error.value = c.error;
    return;
  }
  if (!list.value.some((f) => f.path === c.path)) added.value.push({ path: c.path, repos: null, suggested: false });
  typed.value = '';
  adding.value = false;
  error.value = null;
  tick(c.path, true);
}
</script>

<template>
  <div class="flex flex-col">
    <label
      v-for="f in list"
      :key="f.path"
      class="flex min-h-touch cursor-pointer items-center gap-3 border-t border-line"
      data-testid="folder"
    >
      <input
        type="checkbox"
        class="m-0 size-[18px] accent-ink"
        :checked="roots.includes(f.path)"
        @change="tick(f.path, ($event.target as HTMLInputElement).checked)"
      />
      <span class="flex-1 font-mono text-label">{{ displayPath(f.path, home) }}</span>
      <span v-if="concern(f.path)" class="text-caption text-accent-text">broad</span>
      <span v-else-if="f.repos !== null" class="text-caption text-text-secondary">{{ f.repos }} repo{{ f.repos === 1 ? '' : 's' }}</span>
    </label>
    <p v-if="list.length === 0" class="m-0 border-t border-line py-3 text-label text-text-secondary">
      No usual project folders found. Add the ones you use.
    </p>
  </div>

  <div
    v-if="pending"
    role="alertdialog"
    aria-labelledby="broad-title"
    aria-describedby="broad-desc"
    class="flex flex-col gap-2 rounded-card border border-accent-tint-border bg-accent-tint p-4"
    data-testid="broad-confirm"
  >
    <span id="broad-title" class="font-semibold">Scan all of <span class="font-mono">{{ displayPath(pending, home) }}</span>?</span>
    <span id="broad-desc" class="text-label text-text-strong-muted">
      It {{ concern(pending) }}. Scanning it takes longer and walks through personal files. scull only reads
      metadata and never cleans anything without your review.
    </span>
    <div class="flex flex-wrap gap-2">
      <button ref="confirmBtn" type="button" class="btn-ink" @click="confirmBroad(true)">Scan it anyway</button>
      <button type="button" class="btn-outline" @click="confirmBroad(false)">Keep it off</button>
    </div>
  </div>

  <form v-if="adding" class="flex flex-col gap-2" @submit.prevent="add">
    <label for="add-folder" class="text-label text-text-secondary">Folder path</label>
    <div class="flex gap-2">
      <input
        id="add-folder"
        ref="input"
        v-model="typed"
        type="text"
        spellcheck="false"
        autocomplete="off"
        placeholder="~/Projects"
        :aria-invalid="error ? 'true' : undefined"
        :aria-describedby="error ? 'add-folder-error' : undefined"
        class="min-h-touch min-w-0 flex-1 rounded-button border border-border bg-surface px-3 font-mono text-label text-ink"
      />
      <button type="submit" class="btn-ink">Add</button>
      <button type="button" class="btn-outline" @click="adding = false">Cancel</button>
    </div>
    <span v-if="error" id="add-folder-error" class="text-caption text-accent-text" role="alert">{{ error }}</span>
  </form>
  <button v-else type="button" class="btn-outline self-start" @click="openAdd">Add folder</button>
</template>

<style scoped>
.btn-outline,
.btn-ink {
  min-height: 44px;
  padding: 0 16px;
  border-radius: var(--radius-button);
  font: inherit;
  font-weight: 500;
  cursor: pointer;
}
.btn-outline {
  background: var(--color-surface);
  color: var(--color-ink);
  border: 1px solid var(--color-border);
}
.btn-outline:hover {
  background: var(--color-surface-subtle);
}
.btn-ink {
  background: var(--color-ink);
  color: var(--color-on-ink);
  border: 0;
}
</style>
