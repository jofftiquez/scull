// Design 11's machine for the menu bar widget: 41.2 GB free of 494 GB losing 3.5 GB a day
// (full in about 9 days), a 38.6 GB ready-to-review plan, 14.9 GB of npm/pnpm/Bun/Yarn caches and
// two alerts, checked 4 minutes ago.
//
// MOCK ONLY, OPT-IN: `menubar.html?demo=design` with the mock bridge (screenshots, E2E). It never
// wraps the Tauri bridge, and the real app never sees these numbers.
import type { Alert, CachesReport, DesktopStatus, ForecastsView, ScullBridge } from '../../bridge';
import { forecastsView, steadyForecast } from '../../bridge/forecastMock';
import { DESIGN_CANDIDATES } from '../overview/designScenario';

const GiB = 1024 ** 3;
const DAY = 86_400;
const g = (x: number) => Math.round(x * GiB);

/** Whether the page asked for the design's data (`?demo=design`). */
export function wantsDesign(w: unknown = globalThis): boolean {
  const loc = (w as { location?: { search?: string; hash?: string } } | undefined)?.location;
  if (!loc) return false;
  const q = `${loc.search ?? ''}&${(loc.hash ?? '').split('?')[1] ?? ''}`;
  return /(^|[?&])demo=design(&|$)/.test(q);
}

function alert(kind: Alert['kind'], severity: Alert['severity'], subject: string, message: string, at: number): Alert {
  return { kind, severity, subject, message, key: `${kind}:${subject}`, raised_at: at, bytes: null, paths: [] };
}

export function designMenuBarBridge(base: ScullBridge, nowSecs: () => number = () => Math.floor(Date.now() / 1000)): ScullBridge {
  if (base.kind !== 'mock') return base;
  return {
    ...base,
    async status(): Promise<DesktopStatus> {
      const s = await base.status();
      const now = nowSecs();
      return {
        ...s,
        state: 'ok',
        now,
        last_scan: s.last_scan ? { ...s.last_scan, age_secs: 240, finished_at: now - 240, safe_reclaimable_bytes: g(38.6) } : null,
        volumes: [{ volume_id: '/', total_bytes: g(494), free_bytes: g(41.2), sampled_at: now - 240 }],
        review_plan: {
          plan_id: 'plan_01HZXDESIGN11',
          file: '/Users/ada/Library/Application Support/scull/review-plan.json',
          items: 14,
          reclaimable_bytes: g(38.6),
          created_at: now - 240,
          expires_at: now + DAY,
          includes_s1: false,
          review_command: 'scull clean --plan "/Users/ada/Library/Application Support/scull/review-plan.json"',
        },
        alerts: [
          alert('worktree_ttl_expired', 'critical', '/Users/ada/code/.worktrees', '22 agent worktrees past their TTL', now - 240),
          alert('budget_exceeded', 'warning', 'ml_cache', 'Hugging Face cache over budget', now - 240),
        ],
      };
    },
    async caches(opts): Promise<CachesReport> {
      const r = await base.caches(opts);
      return { ...r, candidates: structuredClone(DESIGN_CANDIDATES.filter((c) => c.category === 'package_cache')) };
    },
    async forecasts(): Promise<ForecastsView> {
      // The design's disk: 41.2 GB free four minutes ago, losing 3.5 GB a day (about 9 days).
      const now = nowSecs();
      return forecastsView(now, [steadyForecast({ volumeId: '/', totalGiB: 494, freeGiB: 41.2, perDayGiB: 3.5, asOf: now - 240 })]);
    },
  };
}
