// Shell-wide state shared by the sidebar and the header. Screens fill it in as the core
// bridge reports scans (P3-APP-02); until then the shell shows its neutral first-run text.
import { computed, reactive } from 'vue';
import type { NavIconName } from '../navigation';

export interface ShellState {
  /** The header's freshness line, e.g. "scanned 4 min ago · 3 folders · …". */
  status: string;
  /** Figures next to nav items. */
  counts: Partial<Record<NavIconName, string>>;
  version: string;
  /** Bumped on success moments (Quick Clean done): the sidebar logo winks (D-20). */
  wink: number;
}

export const NOT_SCANNED = 'not scanned yet';

export const shell = reactive<ShellState>({
  status: NOT_SCANNED,
  counts: {},
  version: '0.0.0',
  wink: 0,
});

export function useShellStatus() {
  return computed(() => shell.status);
}
