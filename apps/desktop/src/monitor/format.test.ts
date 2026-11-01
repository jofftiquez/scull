import { describe, expect, it } from 'vitest';
import { formatWhen, secsUntilTomorrowMorning } from './format';

describe('monitor times', () => {
  it('says today, tomorrow or the weekday, in local time', () => {
    const now = new Date(2026, 10, 2, 10, 0, 0); // Monday
    const at = (d: Date) => d.getTime() / 1000;
    expect(formatWhen(at(new Date(2026, 10, 2, 14, 5)), now)).toBe('today 14:05');
    expect(formatWhen(at(new Date(2026, 10, 3, 8, 0)), now)).toBe('tomorrow 08:00');
    expect(formatWhen(at(new Date(2026, 10, 9, 9, 0)), now)).toBe('Mon 09:00');
  });

  it('pauses until tomorrow 08:00', () => {
    expect(secsUntilTomorrowMorning(new Date(2026, 10, 2, 22, 0, 0))).toBe(10 * 3600);
  });
});
