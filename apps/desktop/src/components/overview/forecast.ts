// "Disk full in about N days" (design 01 forecast alert). The days come from the core's
// forecast (src/lib/forecast.ts: the `forecasts` bridge command, the same source as Insights,
// the menu bar widget, `scull check`/`status` and the weekly digest). The alert shows only a
// forecast the core calls actionable (filling with at least medium confidence, or below the
// reserve): otherwise it stays hidden rather than guessing (PRD principle: never fabricate).

/** The alert shows from this many days out. */
export const ALERT_DAYS = 30;

/** Ring fill of the alert (design: 113 = the r=18 circumference, full at 30 days). */
export function daysDash(days: number): string {
  return Math.min(113, (days / ALERT_DAYS) * 113).toFixed(1);
}
