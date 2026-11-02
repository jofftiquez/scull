<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, ref, watch } from 'vue';
import { toBridgeError } from '../../bridge';
import { broadRootConcern, checkRoot, displayPath, type DetectedFolder } from './model';

const props = defineProps<{ folders: DetectedFolder[]; home: string; pickFolders: () => Promise<string[] | null> }>();
const roots = defineModel<string[]>('roots', { required: true });
const confirmed = defineModel<string[]>('confirmed', { required: true });
const emit = defineEmits<{ busy: [boolean] }>();
const added = ref<DetectedFolder[]>([]);
const removed = defineModel<string[]>('removed', { default: () => [] });
const list = computed(() => {
  const all = [...props.folders, ...added.value, ...roots.value.map((path) => ({ path, repos: null, suggested: false }))];
  return all.filter((f, i) => !removed.value.includes(f.path) && all.findIndex((x) => x.path === f.path) === i);
});
const pending = ref<string[]>([]);
const picking = ref(false);
const error = ref<string | null>(null);
const confirmBtn = ref<HTMLButtonElement | null>(null);
const chooseBtn = ref<HTMLButtonElement | null>(null);
let alive = true;
onBeforeUnmount(() => { alive = false; emit('busy', false); });
watch(() => picking.value || pending.value.length > 0, (value) => emit('busy', value));
watch(() => pending.value[0], async (path) => {
  await nextTick();
  if (path) confirmBtn.value?.focus();
  else chooseBtn.value?.focus();
});
const concern = (p: string) => broadRootConcern(p, props.home);

function setRoot(path: string, on: boolean) {
  const selected = new Set(roots.value.filter((r) => r !== path));
  if (on) selected.add(path);
  roots.value = list.value.map((f) => f.path).filter((p) => selected.has(p));
  if (!on) confirmed.value = confirmed.value.filter((r) => r !== path);
}

function tick(path: string, on: boolean) {
  if (on && concern(path) && !confirmed.value.includes(path)) {
    if (!pending.value.includes(path)) pending.value.push(path);
  } else setRoot(path, on);
}

function remove(path: string) {
  setRoot(path, false);
  removed.value = [...removed.value, path];
}

function confirmBroad(yes: boolean) {
  const path = pending.value.shift();
  if (!path || !yes) return;
  confirmed.value = [...confirmed.value.filter((r) => r !== path), path];
  setRoot(path, true);
}

async function choose() {
  if (picking.value || pending.value.length || !props.home) return;
  picking.value = true;
  error.value = null;
  try {
    const paths = await props.pickFolders();
    if (!alive || !paths) return;
    const selected = new Set(roots.value);
    for (const path of paths) {
      const checked = checkRoot(path, props.home);
      if (!checked.ok) { error.value = checked.error; continue; }
      removed.value = removed.value.filter((p) => p !== path);
      if (!list.value.some((f) => f.path === path)) added.value.push({ path, repos: null, suggested: false });
      if (concern(path) && !confirmed.value.includes(path)) {
        if (!pending.value.includes(path)) pending.value.push(path);
      } else selected.add(path);
    }
    roots.value = list.value.map((f) => f.path).filter((p) => selected.has(p));
  } catch (e) {
    if (alive) error.value = toBridgeError(e).message;
  } finally {
    if (alive) picking.value = false;
  }
}
</script>

<template>
  <div class="folders">
    <button ref="chooseBtn" type="button" class="btn-ink self-start" data-testid="choose-folders" :disabled="picking || pending.length > 0 || !home" @click="choose">
      {{ picking ? 'Choosing folders…' : 'Choose folders…' }}
    </button>
    <p v-if="list.length === 0" class="folder-empty">Choose the folders where you keep your projects. You can select more than one.</p>
    <div v-if="list.length" class="folder-list">
      <div v-for="f in list" :key="f.path" class="folder-row" data-testid="folder">
        <label class="folder-label">
          <input type="checkbox" :checked="roots.includes(f.path)" :disabled="picking || pending.length > 0" @change="tick(f.path, ($event.target as HTMLInputElement).checked)" />
          <span class="folder-details">
            <span class="font-medium">{{ f.path.split(/[\\/]/).filter(Boolean).pop() || f.path }}</span>
            <span class="folder-path">{{ displayPath(f.path, home) }}</span>
          </span>
        </label>
        <button type="button" class="remove" :disabled="picking || pending.length > 0" :aria-label="`Remove ${displayPath(f.path, home)}`" @click="remove(f.path)">Remove</button>
      </div>
    </div>
    <div v-if="pending[0]" role="alertdialog" aria-modal="false" aria-labelledby="broad-title" aria-describedby="broad-desc" class="broad-confirm" data-testid="broad-confirm">
      <h3 id="broad-title" class="m-0 font-semibold">Scan all of {{ displayPath(pending[0], home) }}?</h3>
      <p id="broad-desc" class="m-0 text-label text-text-secondary">It {{ concern(pending[0]) }}. Scanning it takes longer and includes personal-file metadata. Nothing is cleaned without your review.</p>
      <div class="flex flex-wrap gap-3">
        <button ref="confirmBtn" type="button" class="btn-ink" data-testid="confirm-broad" @click="confirmBroad(true)">Include this folder</button>
        <button type="button" class="btn-outline" data-testid="decline-broad" @click="confirmBroad(false)">Leave it out</button>
      </div>
    </div>
    <p v-if="error" class="m-0 text-accent-text" role="alert">{{ error }} Try choosing your folders again.</p>
  </div>
</template>

<style scoped>
.folders { display: flex; flex-direction: column; gap: 20px; }
.folder-empty { margin: 0; color: var(--color-text-secondary); line-height: 1.6; }
.folder-row { display: flex; align-items: center; gap: 16px; min-width: 0; border-bottom: 1px solid var(--color-line); padding: 16px 0; }
.folder-label { display: flex; align-items: center; gap: 14px; flex: 1; min-width: 0; cursor: pointer; }
.folder-label input { width: 18px; height: 18px; flex-shrink: 0; accent-color: var(--color-ink); }
.folder-details { display: flex; flex-direction: column; gap: 4px; min-width: 0; }
.folder-path { font-family: var(--font-mono); font-size: 12px; color: var(--color-text-secondary); overflow-wrap: anywhere; }
.broad-confirm { display: flex; flex-direction: column; gap: 16px; padding: 20px; border: 1px solid var(--color-accent-tint-border); border-radius: var(--radius-button); background: var(--color-accent-tint); }
.btn-ink, .btn-outline, .remove { min-height: 44px; padding: 0 16px; border-radius: var(--radius-button); font: inherit; cursor: pointer; }
.btn-ink { background: var(--color-ink); color: var(--color-on-ink); border: 1px solid var(--color-ink); }
.btn-outline { background: transparent; color: var(--color-ink); border: 1px solid var(--color-border); }
.remove { background: transparent; border: 0; color: var(--color-text-secondary); }
button:disabled { opacity: .5; cursor: not-allowed; }
button:focus-visible, input:focus-visible { outline: 2px solid var(--color-ink); outline-offset: 4px; }
</style>
