<script setup lang="ts">
// Settings (design 10 / 10b / 10c): seven tabs. Values come from the read-only `settings`
// command (effective config + the origin of each value, built-in protection, scull's own data)
// and `drives`. Changing anything is a mutation, so it goes through the screen-local stub
// (components/settings/bridge.ts): the mock keeps changes in memory; inside Tauri it refuses,
// the control reverts and the reason is shown. `mcp.destructive` is file-only and locked here.
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue';
import { useRoute, useRouter } from 'vue-router';
import {
  toBridgeError,
  useBridge,
  type AppInfo,
  type Drive,
  type ProjectsView,
  type SettingsView,
} from '../../bridge';
import { formatBytes } from '../../lib/format';
import ScreenLayout from '../../components/shell/ScreenLayout.vue';
import SettingRow from '../../components/settings/SettingRow.vue';
import SettingSwitch from '../../components/settings/SettingSwitch.vue';
import SegmentedControl from '../../components/settings/SegmentedControl.vue';
import {
  createPendingSettingsWriter,
  settingsWriterFor,
  type SettingValue,
  type SettingsWriter,
} from '../../components/settings/bridge';
import {
  ACTIVITY_WINDOWS,
  APP_DEFAULTS,
  CAUTION,
  CHECK_EVERY,
  DATA_BUDGET_BYTES,
  DESIGN_BUDGETS,
  DESIGN_CLIENTS,
  HISTORY_KEEP,
  RETENTION,
  TABS,
  activityOption,
  databaseLine,
  driveRows,
  isTab,
  originLabel,
  rootNote,
  tilde,
  type AppKey,
  type TabId,
} from '../../components/settings/model';

const props = defineProps<{ writer?: SettingsWriter }>();

const bridge = useBridge();
const route = useRoute();
const router = useRouter();
// `?demo=pending` (mock only): the writer the app uses inside Tauri today, which refuses.
const writer =
  props.writer ??
  (bridge.kind === 'mock' && route?.query.demo === 'pending' ? createPendingSettingsWriter() : settingsWriterFor(bridge));
const demo = bridge.kind === 'mock';

const view = ref<SettingsView | null>(null);
const drives = ref<Drive[]>([]);
const projects = ref<ProjectsView | null>(null);
const info = ref<AppInfo | null>(null);
const loadError = ref('');

const tab = ref<TabId>(isTab(route?.query.tab) ? route.query.tab : 'general');
watch(
  () => route?.query.tab,
  (t) => {
    if (isTab(t)) tab.value = t;
  },
);

/** Values changed in this session (saved through the writer). */
const changed = ref<Map<string, SettingValue>>(new Map());
const saved = ref(false);
const saveError = ref('');
const busy = ref<string | null>(null);
const notice = ref('');
const newRoot = ref<string | null>(null);
const newProtect = ref<string | null>(null);
const showFiles = ref(false);
const connected = ref<Map<string, boolean>>(new Map(DESIGN_CLIENTS.map((c) => [c.id, c.connected])));
let savedTimer: ReturnType<typeof setTimeout> | undefined;

onMounted(async () => {
  try {
    const [s, d, p, a] = await Promise.all([bridge.settings(), bridge.drives(), bridge.projects(), bridge.appInfo()]);
    view.value = s;
    drives.value = d;
    projects.value = p;
    info.value = a;
  } catch (e) {
    loadError.value = toBridgeError(e).message;
  }
});
onBeforeUnmount(() => clearTimeout(savedTimer));

function flashSaved() {
  saved.value = true;
  clearTimeout(savedTimer);
  savedTimer = setTimeout(() => (saved.value = false), 1500);
}

/** Change one setting: optimistic, reverted (with the reason) if the writer refuses. */
async function set(key: string, value: SettingValue) {
  if (busy.value) return;
  const before = new Map(changed.value);
  changed.value = new Map([...changed.value, [key, value]]);
  busy.value = key;
  saveError.value = '';
  notice.value = '';
  try {
    await writer.save(key, value);
    flashSaved();
  } catch (e) {
    changed.value = before;
    saved.value = false;
    saveError.value = `Not saved: ${toBridgeError(e).message}`;
  } finally {
    busy.value = null;
  }
}

async function act(what: () => Promise<void>, done: string, after?: () => void) {
  saveError.value = '';
  notice.value = '';
  try {
    await what();
    after?.();
    notice.value = done;
    flashSaved();
  } catch (e) {
    saveError.value = toBridgeError(e).message;
  }
}

const app = <K extends AppKey>(k: K) => (changed.value.get(k) ?? APP_DEFAULTS[k]) as (typeof APP_DEFAULTS)[K];
const cfg = computed(() => view.value?.config);
const roots = computed(() => (changed.value.get('scan.roots') as string[] | undefined) ?? cfg.value?.scan?.roots ?? []);
const protect = computed(() => (changed.value.get('scan.protect') as string[] | undefined) ?? cfg.value?.scan?.protect ?? []);
const activity = computed(
  () => (changed.value.get('scan.activity_window') as string | undefined) ?? activityOption(cfg.value?.scan?.activity_window),
);
const mcp = (k: 'enabled' | 'redact_home' | 'hash_names'): boolean =>
  (changed.value.get(`mcp.${k}`) as boolean | undefined) ?? cfg.value?.mcp?.[k] ?? (k === 'redact_home');
/** Origin of a config key; a value changed here is now in config.toml. */
const origin = (key: string) => (changed.value.has(key) ? 'config.toml' : originLabel(view.value?.origins[key]));

const driveOverrides = computed(() => {
  const m = new Map<string, boolean>();
  for (const [k, v] of changed.value) if (k.startsWith('drives.')) m.set(k.slice(7), v as boolean);
  return m;
});
const driveList = computed(() => driveRows(drives.value, driveOverrides.value));

function pickTab(t: TabId) {
  tab.value = t;
  saveError.value = '';
  notice.value = '';
  void router?.replace({ query: { ...route.query, tab: t } }).catch(() => undefined);
}

/** Up/down arrows move between tabs (vertical tablist). */
function onTabKey(e: KeyboardEvent, i: number) {
  const d = e.key === 'ArrowDown' ? 1 : e.key === 'ArrowUp' ? -1 : 0;
  if (!d) return;
  e.preventDefault();
  const next = TABS[(i + d + TABS.length) % TABS.length]!;
  pickTab(next.id);
  const list = (e.currentTarget as HTMLElement).parentElement;
  list?.querySelectorAll<HTMLButtonElement>('[role="tab"]')[TABS.indexOf(next)]?.focus();
}

function addPath(kind: 'scan.roots' | 'scan.protect', input: string | null, current: string[]) {
  const p = (input ?? '').trim().replace(/[\\/]+$/, '');
  if (!p) return;
  if (!/^(~|\/|[A-Za-z]:\\)/.test(p)) {
    saveError.value = 'Use an absolute folder or one under your home (~/…).';
    return;
  }
  if (current.includes(p)) {
    saveError.value = `${p} is already listed.`;
    return;
  }
  if (kind === 'scan.roots') newRoot.value = null;
  else newProtect.value = null;
  void set(kind, [...current, p]);
}

const footprintPct = computed(() => Math.min(100, ((view.value?.data.footprint_bytes ?? 0) / DATA_BUDGET_BYTES) * 100));
const versionText = computed(() => info.value?.version ?? '');
</script>

<template>
  <ScreenLayout>
    <header class="flex flex-wrap items-center justify-between gap-4 rounded-tile bg-surface px-6 py-5">
      <div class="flex flex-col gap-0.5">
        <h1 class="m-0 text-h1 font-semibold tracking-[-0.02em]">Settings</h1>
        <span class="text-label text-text-secondary">Changes save automatically</span>
      </div>
      <span
        role="status"
        aria-live="polite"
        class="inline-flex min-h-[32px] items-center gap-1.5 rounded-chip bg-violet-tint px-3 text-label font-semibold text-violet-text transition-[opacity,transform] duration-[250ms] ease-[ease]"
        :style="{ opacity: saved ? 1 : 0, transform: `translateY(${saved ? 0 : -6}px)` }"
        data-testid="saved"
      >{{ saved ? 'Saved ✓' : '' }}</span>
    </header>

    <section v-if="loadError" role="alert" class="rounded-tile border border-accent-tint-border bg-accent-tint p-tile-padding">
      <h2 class="m-0 text-h3 font-semibold">Settings unavailable</h2>
      <p class="m-0 text-label text-text-strong-muted">{{ loadError }}</p>
    </section>

    <div v-else-if="view" class="flex flex-wrap items-start gap-page-gap">
      <div role="tablist" aria-label="Settings sections" aria-orientation="vertical" class="flex max-w-[240px] flex-[1_1_200px] flex-col gap-0.5 rounded-tile bg-surface p-2">
        <button
          v-for="(t, i) in TABS"
          :id="`settings-tab-${t.id}`"
          :key="t.id"
          type="button"
          role="tab"
          :aria-selected="tab === t.id ? 'true' : 'false'"
          :aria-controls="`settings-panel-${t.id}`"
          :tabindex="tab === t.id ? 0 : -1"
          class="min-h-[44px] cursor-pointer rounded-button border-0 px-3.5 text-left font-medium transition-[background] duration-200"
          :class="tab === t.id ? 'bg-ink text-on-ink' : 'bg-transparent text-ink hover:bg-surface-subtle'"
          @click="pickTab(t.id)"
          @keydown="onTabKey($event, i)"
        >
          {{ t.label }}
        </button>
      </div>

      <div
        :id="`settings-panel-${tab}`"
        role="tabpanel"
        :aria-labelledby="`settings-tab-${tab}`"
        class="flex min-w-0 flex-[999_1_420px] flex-col gap-page-gap"
        :data-testid="`panel-${tab}`"
      >
        <p v-if="view.config_state === 'invalid'" role="alert" class="m-0 rounded-card border border-accent-tint-border bg-accent-tint px-4 py-3 text-label text-text-strong-muted" data-testid="config-error">
          The config file can’t be used, so these are the built-in defaults: {{ view.config_error }}
        </p>
        <p v-if="saveError" role="alert" class="m-0 rounded-card border border-accent-tint-border bg-accent-tint px-4 py-3 text-label text-accent-text" data-testid="save-error">{{ saveError }}</p>
        <p v-if="notice" role="status" class="m-0 rounded-card bg-violet-tint px-4 py-3 text-label text-violet-text" data-testid="notice">{{ notice }}</p>

        <!-- General (design 10) -->
        <template v-if="tab === 'general'">
          <section class="settings-section">
            <h2>Scanning</h2>
            <SettingRow title="Scan folders" description="scull only looks inside these. Never your whole disk by default." :origin="origin('scan.roots')" />
            <ul class="m-0 list-none p-0" data-testid="roots">
              <li v-if="!roots.length" class="border-t border-line py-3.5 text-label text-text-secondary">No scan folders yet: add one, or pick them in onboarding.</li>
              <li v-for="r in roots" :key="r" class="flex items-center gap-4 border-t border-line py-2">
                <span class="flex-1 font-mono text-label [overflow-wrap:anywhere]">{{ tilde(r) }}</span>
                <span class="text-caption text-text-tertiary">{{ rootNote(r, projects) }}</span>
                <button type="button" class="min-h-[44px] cursor-pointer rounded-small border-0 bg-transparent px-2.5 text-caption font-medium text-text-strong-muted hover:text-ink" :aria-label="`Remove ${tilde(r)}`" @click="set('scan.roots', roots.filter((x) => x !== r))">Remove</button>
              </li>
            </ul>
            <form v-if="newRoot !== null" class="flex flex-wrap items-center gap-2 py-2" @submit.prevent="addPath('scan.roots', newRoot, roots)">
              <input v-model="newRoot" aria-label="Folder to scan" placeholder="~/projects" class="min-h-[44px] min-w-0 flex-1 rounded-button border border-border bg-surface px-3 font-mono text-label outline-none focus:border-ink" />
              <button type="submit" class="min-h-[44px] cursor-pointer rounded-button border-0 bg-ink px-4 text-label font-semibold text-on-ink">Add</button>
              <button type="button" class="min-h-[44px] cursor-pointer rounded-button border-0 bg-transparent px-3 text-label text-text-strong-muted" @click="newRoot = null">Cancel</button>
            </form>
            <div v-else class="pt-1.5 pb-4">
              <button type="button" class="settings-outline" @click="newRoot = ''">Add folder</button>
            </div>
          </section>
          <section class="settings-section">
            <h2>Schedule</h2>
            <SettingRow title="Check every" description="Light check of free space and known caches, run by the OS scheduler, not a background app.">
              <SegmentedControl label="Check every" :options="CHECK_EVERY" :value="app('app.check_every')" @pick="set('app.check_every', $event)" />
            </SettingRow>
            <SettingRow title="Menu bar widget" description="Free space, forecast and Quick Clean from the menu bar.">
              <SettingSwitch label="Menu bar widget" :on="app('app.menu_bar')" @toggle="set('app.menu_bar', !app('app.menu_bar'))" />
            </SettingRow>
            <SettingRow title="Open at login" description="Only the menu bar widget starts; scanning stays on schedule.">
              <SettingSwitch label="Open at login" :on="app('app.open_at_login')" @toggle="set('app.open_at_login', !app('app.open_at_login'))" />
            </SettingRow>
          </section>
        </template>

        <!-- Drives (design 10b) -->
        <template v-else-if="tab === 'drives'">
          <section class="settings-section">
            <h2>Drives</h2>
            <p v-if="!driveList.length" class="m-0 border-t border-line py-3.5 text-label text-text-secondary">No drives found.</p>
            <div v-for="d in driveList" :key="d.key" class="flex items-center gap-3.5 border-t border-line py-3" data-testid="drive-row">
              <span class="flex h-[40px] w-[40px] flex-none items-center justify-center rounded-button" :class="d.on ? 'bg-ink text-on-ink' : 'bg-surface-raised text-text-tertiary'" aria-hidden="true">
                <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><rect x="3" y="6" width="18" height="12" rx="3" /><circle cx="17" cy="12" r="1.2" /></svg>
              </span>
              <span class="flex min-w-0 flex-1 flex-col gap-0.5">
                <span class="font-semibold">{{ d.name }} <span class="font-normal text-text-secondary">· {{ d.kind }}</span></span>
                <span class="text-label text-text-secondary">{{ d.sub }}</span>
              </span>
              <span class="text-caption text-text-tertiary">{{ d.mode }}</span>
              <SettingSwitch :label="`Scan ${d.name}`" :on="d.on" :locked="d.locked" @toggle="set(`drives.${d.key}`, !d.on)" />
            </div>
            <div class="pb-4" />
          </section>
          <section class="settings-section pb-6">
            <h2>How scull treats each drive</h2>
            <div class="grid grid-cols-1 gap-x-6 gap-y-3 pt-2 text-label text-text-strong-muted md:grid-cols-2">
              <span><strong class="text-ink">Own forecast.</strong> Each drive gets its own free-space trend and alerts.</span>
              <span><strong class="text-ink">Own quarantine.</strong> Items never move between drives; undo stays on the same disk.</span>
              <span><strong class="text-ink">External drives</strong> are scanned when connected. Unplugging mid-scan just pauses it.</span>
              <span><strong class="text-ink">Network and backup drives</strong> are skipped: slow to scan and not yours to clean.</span>
            </div>
          </section>
        </template>

        <!-- Safety -->
        <template v-else-if="tab === 'safety'">
          <section class="settings-section">
            <h2>Safety</h2>
            <SettingRow title="Recently used means" description="Anything modified this recently is treated one tier more carefully." :origin="origin('scan.activity_window')">
              <SegmentedControl label="Recently used means" :options="ACTIVITY_WINDOWS" :value="activity" @pick="set('scan.activity_window', $event)" />
            </SettingRow>
            <SettingRow title="How cautious" description="Changes what is preselected. Safety checks are identical at every level.">
              <SegmentedControl label="How cautious" :options="CAUTION" :value="app('app.caution')" @pick="set('app.caution', $event)" />
            </SettingRow>
            <SettingRow title="Quarantine keeps items for" description="Costly and uncertain items wait here before being freed.">
              <SegmentedControl label="Quarantine keeps items for" :options="RETENTION" :value="app('app.quarantine_retention')" @pick="set('app.quarantine_retention', $event)" />
            </SettingRow>
            <SettingRow title="Type to confirm permanent deletes" description="Required for costly or uncertain items that skip quarantine.">
              <SettingSwitch label="Type to confirm permanent deletes" :on="app('app.type_to_confirm')" @toggle="set('app.type_to_confirm', !app('app.type_to_confirm'))" />
            </SettingRow>
          </section>
          <section class="settings-section">
            <h2>Protected folders</h2>
            <span class="pb-2 text-label text-text-secondary">Never touched, even if something inside looks regenerable.</span>
            <ul class="m-0 list-none p-0" data-testid="protected">
              <li v-for="p in view.builtin_protected" :key="`b-${p}`" class="flex min-h-[52px] items-center gap-4 border-t border-line py-2">
                <span class="flex-1 font-mono text-label [overflow-wrap:anywhere]">{{ tilde(p) }}</span>
                <span class="text-caption text-text-tertiary">built in</span>
              </li>
              <li v-for="p in protect" :key="`u-${p}`" class="flex items-center gap-4 border-t border-line py-2">
                <span class="flex-1 font-mono text-label [overflow-wrap:anywhere]">{{ tilde(p) }}</span>
                <span class="text-caption text-text-tertiary">added by you · {{ origin('scan.protect') }}</span>
                <button type="button" class="min-h-[44px] cursor-pointer rounded-small border-0 bg-transparent px-2.5 text-caption font-medium text-text-strong-muted hover:text-ink" :aria-label="`Unprotect ${tilde(p)}`" @click="set('scan.protect', protect.filter((x) => x !== p))">Unprotect</button>
              </li>
            </ul>
            <form v-if="newProtect !== null" class="flex flex-wrap items-center gap-2 py-2" @submit.prevent="addPath('scan.protect', newProtect, protect)">
              <input v-model="newProtect" aria-label="Folder to protect" placeholder="~/code/important" class="min-h-[44px] min-w-0 flex-1 rounded-button border border-border bg-surface px-3 font-mono text-label outline-none focus:border-ink" />
              <button type="submit" class="min-h-[44px] cursor-pointer rounded-button border-0 bg-ink px-4 text-label font-semibold text-on-ink">Protect</button>
              <button type="button" class="min-h-[44px] cursor-pointer rounded-button border-0 bg-transparent px-3 text-label text-text-strong-muted" @click="newProtect = null">Cancel</button>
            </form>
            <div v-else class="pt-1.5 pb-4">
              <button type="button" class="settings-outline" @click="newProtect = ''">Add folder</button>
            </div>
          </section>
        </template>

        <!-- Alerts -->
        <template v-else-if="tab === 'alerts'">
          <section class="settings-section">
            <h2>Alerts</h2>
            <SettingRow title="Warn me when the disk is" description="Forecast-based, so you hear about it days ahead.">
              <span class="flex items-center gap-3">
                <input
                  type="range"
                  min="3"
                  max="30"
                  :value="app('app.alert_days')"
                  aria-label="Days before full"
                  class="h-[44px] w-40 cursor-pointer accent-[var(--color-accent)]"
                  @change="set('app.alert_days', Number(($event.target as HTMLInputElement).value))"
                />
                <span class="min-w-[86px] font-mono text-label text-accent-text">{{ app('app.alert_days') }} days out</span>
              </span>
            </SettingRow>
            <SettingRow title="Sudden growth" description="Alert when something grows more than 20 GB in a day.">
              <SettingSwitch label="Sudden growth" :on="app('app.sudden_growth')" @toggle="set('app.sudden_growth', !app('app.sudden_growth'))" />
            </SettingRow>
            <SettingRow title="Weekly digest" description="A short local notification every Monday.">
              <SettingSwitch label="Weekly digest" :on="app('app.weekly_digest')" @toggle="set('app.weekly_digest', !app('app.weekly_digest'))" />
            </SettingRow>
          </section>
          <section class="settings-section pb-5">
            <h2>Budgets</h2>
            <p v-if="!demo" class="m-0 border-t border-line py-3.5 text-label text-text-secondary">No budgets yet. Budgets for folders and caches arrive with Insights.</p>
            <div v-for="b in demo ? DESIGN_BUDGETS : []" :key="b.name" class="flex flex-col gap-1.5 border-t border-line py-3" data-testid="budget">
              <div class="flex items-baseline justify-between gap-3">
                <span class="font-semibold">{{ b.name }}</span>
                <span class="font-mono text-label" :class="b.usedBytes > b.capBytes ? 'text-accent-text' : 'text-text-secondary'">{{ formatBytes(b.usedBytes) }} / {{ formatBytes(b.capBytes) }}</span>
              </div>
              <div class="h-[6px] overflow-hidden rounded-chip bg-surface-raised">
                <div class="h-full rounded-chip" :style="{ width: `${Math.min(100, (b.usedBytes / b.capBytes) * 100)}%`, background: b.usedBytes > b.capBytes ? 'var(--color-accent)' : 'var(--color-ink)' }" />
              </div>
            </div>
          </section>
        </template>

        <!-- AI agents (design 10c, PRD §13) -->
        <template v-else-if="tab === 'agents'">
          <section class="settings-section">
            <h2>AI agents</h2>
            <template v-if="demo">
              <div v-for="c in DESIGN_CLIENTS" :key="c.id" class="flex items-center gap-3.5 border-t border-line py-3" data-testid="client">
                <span class="flex h-[36px] w-[36px] flex-none items-center justify-center rounded-small bg-surface-raised font-mono text-[11px] font-medium" aria-hidden="true">{{ c.initials }}</span>
                <span class="flex min-w-0 flex-1 flex-col">
                  <span class="font-semibold">{{ c.name }}</span>
                  <span class="text-label text-text-secondary">{{ connected.get(c.id) ? `Connected · ${c.tools}` : 'Found on this machine · not connected' }}</span>
                </span>
                <button
                  v-if="connected.get(c.id)"
                  type="button"
                  class="settings-outline"
                  @click="act(() => writer.disconnectAgent(c.id), `${c.name} disconnected.`, () => connected.set(c.id, false))"
                >
                  Disconnect
                </button>
                <button
                  v-else
                  type="button"
                  class="min-h-[44px] cursor-pointer rounded-button border-0 bg-ink px-3.5 text-label font-semibold text-on-ink"
                  @click="act(() => writer.connectAgent(c.id), `${c.name} connected: read-only tools.`, () => connected.set(c.id, true))"
                >
                  Connect
                </button>
              </div>
            </template>
            <p v-else class="m-0 border-t border-line py-3.5 text-label text-text-secondary">
              Agents connect through <code class="font-mono">scull mcp serve</code> in their MCP settings; connected clients will be listed here.
            </p>
            <SettingRow title="Let agents use scull" description="Read-only tools over stdio, no network listener. Off: scull mcp serve refuses to start." :origin="origin('mcp.enabled')">
              <SettingSwitch label="Let agents use scull" :on="mcp('enabled')" :busy="busy === 'mcp.enabled'" @toggle="set('mcp.enabled', !mcp('enabled'))" />
            </SettingRow>
            <SettingRow
              title="Let agents prepare and run cleanup plans"
              description="Off. Only config.toml plus the --allow-destructive launch flag can turn this on. You approve every plan; agents can never approve their own."
              :origin="origin('mcp.destructive')"
            >
              <SettingSwitch label="Let agents prepare and run cleanup plans" :on="!!cfg?.mcp?.destructive" locked />
            </SettingRow>
            <SettingRow title="Hide my home folder in paths" description="Agents see ~/code instead of /Users/you/code." :origin="origin('mcp.redact_home')">
              <SettingSwitch label="Hide my home folder in paths" :on="mcp('redact_home')" @toggle="set('mcp.redact_home', !mcp('redact_home'))" />
            </SettingRow>
            <SettingRow title="Hash project names" description="Agents see p-3fa2 instead of real project names." :origin="origin('mcp.hash_names')">
              <SettingSwitch label="Hash project names" :on="mcp('hash_names')" @toggle="set('mcp.hash_names', !mcp('hash_names'))" />
            </SettingRow>
            <SettingRow title="Results per tool call" description="Caps what one answer can send to the agent. File contents are never sent." :origin="origin('mcp.max_results')">
              <span class="font-mono text-label" data-testid="max-results">{{ cfg?.mcp?.max_results ?? 1000 }}</span>
            </SettingRow>
            <div class="pb-2" />
          </section>
        </template>

        <!-- Data & privacy (scull data show, read-only) -->
        <template v-else-if="tab === 'data'">
          <section class="settings-section">
            <h2>Your data</h2>
            <div class="flex flex-col gap-2 border-t border-line py-3.5">
              <div class="flex items-baseline justify-between gap-3">
                <span class="font-semibold" data-testid="footprint">{{ formatBytes(view.data.footprint_bytes) }} of history</span>
                <span class="text-label text-text-secondary">budget {{ formatBytes(DATA_BUDGET_BYTES) }}</span>
              </div>
              <div class="h-[6px] overflow-hidden rounded-chip bg-surface-raised">
                <div class="h-full rounded-chip bg-ink" :style="{ width: `${footprintPct}%` }" />
              </div>
              <span class="text-label text-text-secondary">Stored only on this machine. No telemetry, ever. File contents and secrets are never recorded.</span>
            </div>
            <SettingRow title="Keep detailed history for" description="Older data is summarized, then kept as monthly totals.">
              <SegmentedControl label="Keep detailed history for" :options="HISTORY_KEEP" :value="app('app.history_keep')" @pick="set('app.history_keep', $event)" />
            </SettingRow>
            <div class="flex flex-wrap gap-2 border-t border-line py-3.5">
              <button type="button" class="settings-outline" @click="act(() => writer.exportData(), 'Export requested.')">Export everything</button>
              <button type="button" class="settings-outline" :aria-expanded="showFiles ? 'true' : 'false'" aria-controls="what-scull-knows" @click="showFiles = !showFiles">What scull knows</button>
              <button type="button" class="settings-outline text-accent-text" @click="act(() => writer.deleteHistory(), 'Delete requested (the mock deletes nothing).')">Delete all history</button>
            </div>
            <div v-if="showFiles" id="what-scull-knows" class="flex flex-col gap-1 border-t border-line py-3.5 text-label" data-testid="what-scull-knows">
              <span><span class="text-text-secondary">Database:</span> {{ databaseLine(view) }}</span>
              <span v-if="view.data.data_dir"><span class="text-text-secondary">Data folder:</span> <span class="font-mono [overflow-wrap:anywhere]">{{ tilde(view.data.data_dir) }}</span></span>
              <span v-if="view.data.config_dir"><span class="text-text-secondary">Config folder:</span> <span class="font-mono [overflow-wrap:anywhere]">{{ tilde(view.data.config_dir) }}</span></span>
              <ul class="m-0 mt-1 list-none p-0">
                <li v-for="f in view.data.files" :key="f.path" class="flex justify-between gap-3 py-0.5 font-mono text-caption">
                  <span class="[overflow-wrap:anywhere]">{{ tilde(f.path) }} <span class="text-text-tertiary">· {{ f.kind }}</span></span>
                  <span>{{ formatBytes(f.bytes) }}</span>
                </li>
              </ul>
            </div>
          </section>
        </template>

        <!-- About -->
        <template v-else>
          <section class="settings-section">
            <h2>About scull</h2>
            <div class="flex flex-col gap-0.5 border-t border-line py-3.5">
              <span class="text-h3 font-semibold">scull</span>
              <span class="text-label text-text-secondary" data-testid="version">v{{ versionText }} · MIT OR Apache-2.0 · free and open source</span>
            </div>
            <SettingRow title="Supporter key" description="Unlocks signed auto-updates, early builds and extra skull themes. Every feature stays free.">
              <input type="text" placeholder="SCULL-XXXX-XXXX" aria-label="Supporter key" class="min-h-[44px] w-48 rounded-button border border-border bg-surface px-3 font-mono text-label outline-none focus:border-ink" @change="set('app.supporter_key', ($event.target as HTMLInputElement).value)" />
            </SettingRow>
            <SettingRow title="Check for updates" description="Opt-in. The only network request scull makes on its own.">
              <SettingSwitch label="Check for updates" :on="app('app.check_updates')" @toggle="set('app.check_updates', !app('app.check_updates'))" />
            </SettingRow>
            <SettingRow title="Configuration" :description="view.config_file ? tilde(view.config_file) : 'No config location on this machine'" :origin="view.config_state" />
            <div class="flex flex-wrap gap-4 border-t border-line py-3.5 text-label">
              <a href="https://github.com/jofftiquez/scull" target="_blank" rel="noopener noreferrer" class="inline-flex min-h-[44px] items-center text-ink">Source code</a>
              <a href="https://github.com/jofftiquez/scull/blob/main/SECURITY.md" target="_blank" rel="noopener noreferrer" class="inline-flex min-h-[44px] items-center text-ink">Security policy</a>
              <a href="https://github.com/jofftiquez/scull#license" target="_blank" rel="noopener noreferrer" class="inline-flex min-h-[44px] items-center text-ink">Licenses</a>
            </div>
          </section>
        </template>
      </div>
    </div>
  </ScreenLayout>
</template>

<style scoped>
.settings-section {
  display: flex;
  flex-direction: column;
  border-radius: var(--radius-tile);
  background: var(--color-surface);
  padding: 22px 24px 8px;
  animation: scull-rise 260ms ease-out;
}
.settings-section h2 {
  margin: 0 0 6px;
  font-size: 18px;
  font-weight: 600;
  padding-bottom: 10px;
}
.settings-outline {
  min-height: 44px;
  padding: 0 14px;
  border: 1px solid var(--color-border);
  border-radius: var(--radius-button);
  background: var(--color-surface);
  font: inherit;
  font-size: 13px;
  font-weight: 500;
  cursor: pointer;
}
.settings-outline:hover {
  border-color: var(--color-ink);
}
</style>
