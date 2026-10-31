// Primary navigation (PRD §14.1; design 01 sidebar). The order and grouping follow the design:
// seven work screens, a divider, then History and Settings.
import type { RouteComponent, RouteRecordRaw } from 'vue-router';

export type NavIconName =
  | 'overview'
  | 'projects'
  | 'explorer'
  | 'worktrees'
  | 'caches'
  | 'cleanup'
  | 'insights'
  | 'history'
  | 'settings';

export interface NavItem {
  /** Route name and icon. */
  name: NavIconName;
  label: string;
  path: string;
  /** Issue that builds the screen (placeholder copy until it lands). */
  issue: string;
  group: 'main' | 'footer';
}

export const NAV_ITEMS: readonly NavItem[] = [
  { name: 'overview', label: 'Overview', path: '/', issue: 'P3-UI-01', group: 'main' },
  { name: 'projects', label: 'Projects', path: '/projects', issue: 'P3-UI-06', group: 'main' },
  { name: 'explorer', label: 'Storage Explorer', path: '/explorer', issue: 'P3-UI-07', group: 'main' },
  { name: 'worktrees', label: 'Git Worktrees', path: '/worktrees', issue: 'P3-UI-05', group: 'main' },
  { name: 'caches', label: 'Caches', path: '/caches', issue: 'P3-UI-08', group: 'main' },
  { name: 'cleanup', label: 'Cleanup Review', path: '/cleanup', issue: 'P3-UI-04', group: 'main' },
  { name: 'insights', label: 'Insights', path: '/insights', issue: 'P3-UI-10', group: 'main' },
  { name: 'history', label: 'History', path: '/history', issue: 'P3-UI-09', group: 'footer' },
  { name: 'settings', label: 'Settings', path: '/settings', issue: 'P3-UI-11', group: 'footer' },
];

const screens: Record<NavIconName, () => Promise<RouteComponent | { default: RouteComponent }>> = {
  overview: () => import('./screens/Overview/OverviewScreen.vue'),
  projects: () => import('./screens/Projects/ProjectsScreen.vue'),
  explorer: () => import('./screens/Explorer/ExplorerScreen.vue'),
  worktrees: () => import('./screens/Worktrees/WorktreesScreen.vue'),
  caches: () => import('./screens/Caches/CachesScreen.vue'),
  cleanup: () => import('./screens/CleanupReview/CleanupReviewScreen.vue'),
  insights: () => import('./screens/Insights/InsightsScreen.vue'),
  history: () => import('./screens/History/HistoryScreen.vue'),
  settings: () => import('./screens/Settings/SettingsScreen.vue'),
};

export const routes: RouteRecordRaw[] = [
  ...NAV_ITEMS.map(
    (item): RouteRecordRaw => ({
      path: item.path,
      name: item.name,
      component: screens[item.name],
      meta: { item },
    }),
  ),
  { path: '/:pathMatch(.*)*', redirect: '/' },
];
