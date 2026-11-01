// Times the monitor UI shows, in the user's local time: `Mon 09:00`, `today 14:30`.
const DAYS = ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat'] as const;

const pad = (n: number) => String(n).padStart(2, '0');

/** `today 14:30`, `tomorrow 08:00`, else `Mon 09:00` (local time). */
export function formatWhen(unixSecs: number, now: Date = new Date()): string {
  const d = new Date(unixSecs * 1000);
  const hm = `${pad(d.getHours())}:${pad(d.getMinutes())}`;
  const day = (x: Date) => new Date(x.getFullYear(), x.getMonth(), x.getDate()).getTime();
  const diff = Math.round((day(d) - day(now)) / 86_400_000);
  if (diff === 0) return `today ${hm}`;
  if (diff === 1) return `tomorrow ${hm}`;
  return `${DAYS[d.getDay()]} ${hm}`;
}

/** Seconds from `now` until tomorrow 08:00 local time. */
export function secsUntilTomorrowMorning(now: Date = new Date()): number {
  const t = new Date(now.getFullYear(), now.getMonth(), now.getDate() + 1, 8, 0, 0);
  return Math.max(60, Math.round((t.getTime() - now.getTime()) / 1000));
}
