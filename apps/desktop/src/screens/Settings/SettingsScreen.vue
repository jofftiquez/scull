<script setup lang="ts">
// Settings (design 10 / 10b / 10c): seven tabs. Values come from the read-only `settings`
// command (effective config + the origin of each value, built-in protection, scull's own data)
// and `drives`. Native configuration edits are validated by the core. Unimplemented app
// preferences are disabled; `mcp.destructive` remains file-only and locked here.
import { computed, inject, onBeforeUnmount, onMounted, ref, watch } from 'vue';
import { useRoute, useRouter } from 'vue-router';
import {
  toBridgeError,
  useBridge,
  type AppInfo,
  type Drive,
  type ProjectsView,
  type SettingsView,
} from '../../bridge';
import { ONBOARDING_KEY, onboardingBridgeFor } from '../../components/onboarding/bridge';
import { checkRoot } from '../../components/onboarding/model';
import { driveOf } from '../../components/overview/drives';
import { loadSetup, savedScanParams } from '../../components/onboarding/prefs';
import { formatBytes } from '../../lib/format';
import { THEME_OPTIONS, isThemePref, useTheme } from '../../lib/theme';
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
  ALERT_COOLDOWN,
  APP_DEFAULTS,
  CAUTION,
  CHECK_EVERY,
  DATA_BUDGET_BYTES,
  DESIGN_BUDGETS,
  DESIGN_CLIENTS,
  HISTORY_KEEP,
  MONITOR_EVERY,
  RETENTION,
  TABS,
  activityOption,
  databaseLine,
  driveRows,
  expandRoot,
  isTab,
  originLabel,
  rootNote,
  tilde,
  type AppKey,
  type TabId,
} from '../../components/settings/model';
import type { MonitorStatus } from '../../bridge';
import { monitorControlFor } from '../../monitor/control';
import { isMonitorKey, loadPrefs, saveMonitorPref } from '../../monitor/prefs';
import { formatWhen, secsUntilTomorrowMorning } from '../../monitor/format';

const props = defineProps<{ writer?: SettingsWriter }>();

const bridge = useBridge();
const folders = inject(ONBOARDING_KEY, null) ?? onboardingBridgeFor(bridge);
const home = ref(loadSetup()?.home ?? '');
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
const loading = ref(true);

const tab = ref<TabId>(isTab(route?.query.tab) ? route.query.tab : 'general');
watch(
  () => route?.query.tab,
  (t) => {
    if (isTab(t)) tab.value = t;
  },
);

// Tray monitoring (P5-PRO-01): its keys are app state kept on this computer and sent to the
// monitor (src/monitor), so they save inside Tauri too; everything else goes through the writer.
const monitor = monitorControlFor(bridge);
const monitorStatus = ref<MonitorStatus | null>(null);
// Only the newest status request may land: a refresh started before a pause or resume (e.g. after
// changing "Look every") must not overwrite the status that pause/resume returned.
let statusSeq = 0;
async function loadMonitorStatus(get: () => Promise<MonitorStatus>): Promise<MonitorStatus> {
  const seq = ++statusSeq;
  const st = await get();
  if (seq === statusSeq) monitorStatus.value = st;
  return st;
}

/** Values changed in this session (saved through the writer), and the stored monitor keys. */
const changed = ref<Map<string, SettingValue>>(new Map(Object.entries(loadPrefs())));
const saved = ref(false);
const saveError = ref('');
const busy = ref<string | null>(null);
const notice = ref('');
const picking = ref(false);
const showFiles = ref(false);
const connected = ref<Map<string, boolean>>(new Map(DESIGN_CLIENTS.map((c) => [c.id, c.connected])));
let savedTimer: ReturnType<typeof setTimeout> | undefined;

onMounted(async () => {
  try {
    const [s, d, p, a, context] = await Promise.all([bridge.settings(), bridge.drives(), bridge.projects(), bridge.appInfo(), folders.context().catch(() => null)]);
    if (context?.home) home.value = context.home;
    view.value = s;
    drives.value = d;
    projects.value = p;
    info.value = a;
  } catch (e) {
    loadError.value = toBridgeError(e).message;
  }
  await loadMonitorStatus(() => bridge.monitorStatus()).catch(() => undefined);
  loading.value = false;
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
    if (isMonitorKey(key) && !Array.isArray(value)) {
      await saveMonitorPref(monitor, key, value);
      void loadMonitorStatus(() => bridge.monitorStatus()).catch(() => undefined);
    } else {
      await writer.save(key, value);
      if (bridge.kind === 'tauri') {
        try {
          view.value = await bridge.settings();
          changed.value.delete(key);
        } catch {
          notice.value = 'Saved. Settings could not be refreshed; reopen this page to reload them.';
        }
      }
    }
    if (key.startsWith('scan.') && !notice.value) notice.value = 'Saved. Run Scan to refresh the results.';
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

/** Theme (P5-UI-01): a display preference of this machine, saved locally, not in config. */
const theme = useTheme();
function pickTheme(v: string) {
  if (!isThemePref(v)) return;
  theme.setTheme(v);
  flashSaved();
}

const app = <K extends AppKey>(k: K) => (changed.value.get(k) ?? APP_DEFAULTS[k]) as (typeof APP_DEFAULTS)[K];
const cfg = computed(() => view.value?.config);
const roots = computed(() => {
  const configured = view.value?.origins['scan.roots']?.some((origin) => origin.source !== 'default');
  return (changed.value.get('scan.roots') as string[] | undefined)
    ?? (!configured && bridge.kind === 'tauri' ? savedScanParams(loadSetup())?.roots : undefined)
    ?? cfg.value?.scan?.roots ?? [];
});
const protect = computed(() => (changed.value.get('scan.protect') as string[] | undefined) ?? cfg.value?.scan?.protect ?? []);
const activity = computed(
  () => (changed.value.get('scan.activity_window') as string | undefined) ?? activityOption(cfg.value?.scan?.activity_window),
);
const mcp = (k: 'enabled' | 'redact_home' | 'hash_names'): boolean =>
  (changed.value.get(`mcp.${k}`) as boolean | undefined) ?? cfg.value?.mcp?.[k] ?? (k === 'redact_home');
/** Origin of a config key; a value changed here is now in config.toml. */
const origin = (key: string) => (changed.value.has(key) ? 'config.toml' : originLabel(view.value?.origins[key]));

const driveList = computed(() => driveRows(drives.value, roots.value, home.value));

async function chooseFolders(kind: 'scan.roots' | 'scan.protect', mount?: string) {
  if (busy.value || picking.value) return;
  picking.value = true;
  saveError.value = '';
  try {
    const paths = await folders.pickFolders();
    if (!paths?.length) return;
    for (const path of paths) {
      const checked = checkRoot(path, home.value);
      if (!checked.ok) throw new Error(checked.error);
      if (mount && driveOf(expandRoot(path, home.value), drives.value)?.mount_point !== mount) {
        throw new Error('Choose folders on the selected drive. No folders were changed.');
      }
    }
    const current = kind === 'scan.roots' ? roots.value : protect.value;
    const seen = new Set(current.map((p) => expandRoot(p, home.value)));
    const added = paths.filter((p) => {
      const key = expandRoot(p, home.value);
      if (seen.has(key)) return false;
      seen.add(key);
      return true;
    });
    if (added.length) await set(kind, [...current, ...added]);
  } catch (e) {
    saveError.value = toBridgeError(e).message;
  } finally {
    picking.value = false;
  }
}

async function toggleDrive(mount: string, on: boolean) {
  if (!on) { await chooseFolders('scan.roots', mount); return; }
  await set('scan.roots', roots.value.filter((p) => driveOf(expandRoot(p, home.value), drives.value)?.mount_point !== mount));
}

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


async function pauseAlerts(secs: number | null) {
  saveError.value = '';
  notice.value = '';
  try {
    const st = await loadMonitorStatus(() => monitor.pause(secs));
    notice.value = secs == null ? 'Alerts resumed.' : `Alerts paused until ${formatWhen(st.settings.paused_until ?? 0)}.`;
    flashSaved();
  } catch (e) {
    saveError.value = toBridgeError(e).message;
  }
}

async function sendDigest() {
  await act(
    async () => {
      await monitor.sendDigestNow();
    },
    monitor.kind === 'mock'
      ? "This week's digest is ready (the browser preview shows no notification)."
      : "This week's digest was sent to your notifications.",
  );
}

const pausedUntil = computed(() => (monitorStatus.value?.paused ? (monitorStatus.value.settings.paused_until ?? null) : null));
const nextDigestText = computed(() => {
  const t = monitorStatus.value?.next_digest;
  return app('app.weekly_digest') && t ? `Next digest ${formatWhen(t)}.` : 'Turn on Weekly digest to get it every Monday.';
});

/** The emergency headroom file (PRO-06): read-only here; the CLI creates and releases it. */
const headroomLine = computed(() => {
  const h = view.value?.data.headroom;
  switch (h?.state) {
    case 'ready':
      return `${formatBytes(h.releasable_bytes ?? 0)} reserved for a full disk. Free it with \`scull headroom release\`.`;
    case 'missing':
      return 'The reserve file is gone; `scull headroom release` clears its record.';
    case 'changed':
    case 'unreadable':
    case 'unrecorded':
      return `Something at the reserve's path isn't the file scull created, so scull won't release it${h.detail ? ` (${h.detail})` : ''}.`;
    default:
      return 'Off. `scull headroom create` sets aside 2 GiB you can free when the disk is full.';
  }
});

const footprintPct = computed(() => Math.min(100, ((view.value?.data.footprint_bytes ?? 0) / DATA_BUDGET_BYTES) * 100));
const versionText = computed(() => info.value?.version ?? '');
</script>

<template>
  <ScreenLayout :loading="loading" loading-label="Reading settings…">
    <header class="flex flex-wrap items-center justify-between gap-3 px-2 py-2">
      <div class="flex flex-col gap-0.5">
        <h1 class="m-0 text-h2 font-semibold">Settings</h1>
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
            <div class="pt-1.5 pb-4">
              <button type="button" class="settings-outline" :disabled="picking || !!busy" data-testid="choose-scan-folders" @click="chooseFolders('scan.roots')">{{ picking ? 'Choosing folders…' : 'Choose folders…' }}</button>
            </div>
          </section>
          <section class="settings-section">
            <h2>Schedule</h2>
            <SettingRow :unavailable="!demo" title="Check every" description="Light check of free space and known caches, run by the OS scheduler, not a background app.">
              <SegmentedControl label="Check every" :options="CHECK_EVERY" :value="app('app.check_every')" @pick="set('app.check_every', $event)" />
            </SettingRow>
            <SettingRow :unavailable="!demo" title="Menu bar widget" description="Free space, forecast and Quick Clean from the menu bar.">
              <SettingSwitch label="Menu bar widget" :on="app('app.menu_bar')" @toggle="set('app.menu_bar', !app('app.menu_bar'))" />
            </SettingRow>
            <SettingRow :unavailable="!demo" title="Open at login" description="Only the menu bar widget starts; scanning stays on schedule.">
              <SettingSwitch label="Open at login" :on="app('app.open_at_login')" @toggle="set('app.open_at_login', !app('app.open_at_login'))" />
            </SettingRow>
          </section>
          <section class="settings-section" data-testid="appearance">
            <h2>Appearance</h2>
            <SettingRow title="Theme" description="Follows your system's light or dark mode unless you pick one. Saved on this computer and used by the menu bar widget too.">
              <SegmentedControl label="Theme" :options="THEME_OPTIONS" :value="theme.pref.value" @pick="pickTheme" />
            </SettingRow>
          </section>
        </template>

        <!-- Drives (design 10b) -->
        <template v-else-if="tab === 'drives'">
          <section class="settings-section">
            <h2>Drives</h2>
            <p class="m-0 text-label text-text-secondary">A drive is selected when it contains your scan folders. Turning it off removes those folders from future scans.</p>
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
              <button v-if="!d.locked" type="button" class="settings-outline" :disabled="picking || !!busy" :aria-label="`Choose folders on ${d.name}`" @click="chooseFolders('scan.roots', d.key)">Choose folders…</button>
              <SettingSwitch :label="`Scan ${d.name}`" :on="d.on" :locked="d.locked || !!busy || picking" @toggle="toggleDrive(d.key, d.on)" />
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
            <SettingRow :unavailable="!demo" title="How cautious" description="Changes what is preselected. Safety checks are identical at every level.">
              <SegmentedControl label="How cautious" :options="CAUTION" :value="app('app.caution')" @pick="set('app.caution', $event)" />
            </SettingRow>
            <SettingRow :unavailable="!demo" title="Quarantine keeps items for" description="Costly and uncertain items wait here before being freed.">
              <SegmentedControl label="Quarantine keeps items for" :options="RETENTION" :value="app('app.quarantine_retention')" @pick="set('app.quarantine_retention', $event)" />
            </SettingRow>
            <SettingRow :unavailable="!demo" title="Type to confirm permanent deletes" description="Required for costly or uncertain items that skip quarantine.">
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
            <div class="pt-1.5 pb-4">
              <button type="button" class="settings-outline" :disabled="picking || !!busy" data-testid="choose-protected-folders" @click="chooseFolders('scan.protect')">Choose protected folders…</button>
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
          <section class="settings-section" data-testid="monitoring">
            <h2>Menu bar monitoring</h2>
            <SettingRow title="Look every" description="The menu bar app reads free space and the alerts scull check recorded. It never scans or cleans.">
              <SegmentedControl label="Look every" :options="MONITOR_EVERY" :value="app('app.monitor_every')" @pick="set('app.monitor_every', $event)" />
            </SettingRow>
            <SettingRow title="Repeat an alert after" description="The same alert stays quiet this long, unless it gets worse.">
              <SegmentedControl label="Repeat an alert after" :options="ALERT_COOLDOWN" :value="app('app.alert_cooldown')" @pick="set('app.alert_cooldown', $event)" />
            </SettingRow>
            <SettingRow title="Pause alerts" description="-">
              <template #description>
                <span data-testid="pause-state">{{ pausedUntil ? `Paused until ${formatWhen(pausedUntil)}.` : 'Alerts are on.' }}</span>
              </template>
              <span class="flex flex-wrap gap-2">
                <template v-if="pausedUntil">
                  <button type="button" class="settings-outline" @click="pauseAlerts(null)">Resume</button>
                </template>
                <template v-else>
                  <button type="button" class="settings-outline" @click="pauseAlerts(3600)">1 hour</button>
                  <button type="button" class="settings-outline" @click="pauseAlerts(secsUntilTomorrowMorning())">Until tomorrow</button>
                </template>
              </span>
            </SettingRow>
            <SettingRow title="This week's digest" description="-">
              <template #description>
                <span data-testid="next-digest">{{ nextDigestText }}</span>
              </template>
              <button type="button" class="settings-outline" @click="sendDigest">Send now</button>
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
            <SettingRow :unavailable="!demo" title="Keep detailed history for" description="Older data is summarized, then kept as monthly totals.">
              <SegmentedControl label="Keep detailed history for" :options="HISTORY_KEEP" :value="app('app.history_keep')" @pick="set('app.history_keep', $event)" />
            </SettingRow>
            <SettingRow title="Emergency headroom" description="-">
              <template #description>
                <span data-testid="headroom">{{ headroomLine }}</span>
              </template>
            </SettingRow>
            <div class="flex flex-wrap gap-2 border-t border-line py-3.5">
              <button type="button" :disabled="!demo" class="settings-outline" @click="act(() => writer.exportData(), 'Export requested.')">Export everything</button>
              <button type="button" class="settings-outline" :aria-expanded="showFiles ? 'true' : 'false'" aria-controls="what-scull-knows" @click="showFiles = !showFiles">What scull knows</button>
              <button type="button" :disabled="!demo" class="settings-outline text-accent-text" @click="act(() => writer.deleteHistory(), 'Delete requested (the mock deletes nothing).')">Delete all history</button>
            </div>
            <p v-if="!demo" class="m-0 pb-3 text-caption text-text-secondary">Export and history deletion are not available yet.</p>
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
            <SettingRow :unavailable="!demo" title="Supporter key" description="Unlocks signed auto-updates, early builds and extra skull themes. Every feature stays free.">
              <input type="text" placeholder="SCULL-XXXX-XXXX" aria-label="Supporter key" class="min-h-[44px] w-48 rounded-button border border-border bg-surface px-3 font-mono text-label outline-none focus:border-ink" @change="set('app.supporter_key', ($event.target as HTMLInputElement).value)" />
            </SettingRow>
            <SettingRow :unavailable="!demo" title="Check for updates" description="Opt-in. The only network request scull makes on its own.">
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
.settings-outline:disabled { cursor: not-allowed; opacity: 0.45; }
.settings-outline:hover {
  border-color: var(--color-ink);
}
</style>
