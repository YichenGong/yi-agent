/**
 * Weekday specs: the backend stores a compressed string (`Mon-Fri`,
 * `Sat,Sun`), the UI edits a seven-element selection.
 *
 * Pure functions, no React: the settings panel recomputes these on every
 * keystroke, so they must be cheap and directly testable.
 */

/**
 * Canonical weekday order (Monday first), matching the backend's own
 * compression order. Display uses Chinese single characters; the wire format
 * is always `Mon`…`Sun`.
 */
export const WEEKDAYS = [
  { en: "Mon", zh: "一" },
  { en: "Tue", zh: "二" },
  { en: "Wed", zh: "三" },
  { en: "Thu", zh: "四" },
  { en: "Fri", zh: "五" },
  { en: "Sat", zh: "六" },
  { en: "Sun", zh: "日" },
] as const;

/** `Mon`/`monday` → 0…6; anything else is `undefined`. */
function weekdayIndex(name: string): number | undefined {
  switch (name.trim().toLowerCase()) {
    case "mon":
    case "monday":
      return 0;
    case "tue":
    case "tuesday":
      return 1;
    case "wed":
    case "wednesday":
      return 2;
    case "thu":
    case "thursday":
      return 3;
    case "fri":
    case "friday":
      return 4;
    case "sat":
    case "saturday":
      return 5;
    case "sun":
    case "sunday":
      return 6;
    default:
      return undefined;
  }
}

/**
 * Parse a compressed spec into a seven-element selection.
 *
 * Unrecognised segments (including reversed ranges) are ignored: bad data
 * should select less, never throw.
 */
export function parseDays(spec: string): boolean[] {
  const selected = [false, false, false, false, false, false, false];
  for (const raw of spec.split(",")) {
    const part = raw.trim();
    if (part === "") continue;
    const dash = part.indexOf("-");
    if (dash === -1) {
      const day = weekdayIndex(part);
      if (day !== undefined) selected[day] = true;
      continue;
    }
    const from = weekdayIndex(part.slice(0, dash));
    const to = weekdayIndex(part.slice(dash + 1));
    if (from === undefined || to === undefined || from > to) continue;
    for (let day = from; day <= to; day += 1) selected[day] = true;
  }
  return selected;
}

/** {@link parseDays} inverted: compress a selection back to the canonical spec. */
export function renderDays(selected: boolean[]): string {
  const parts: string[] = [];
  let i = 0;
  while (i < WEEKDAYS.length) {
    if (!selected[i]) {
      i += 1;
      continue;
    }
    const start = i;
    while (i + 1 < WEEKDAYS.length && selected[i + 1]) i += 1;
    parts.push(i === start ? WEEKDAYS[start].en : `${WEEKDAYS[start].en}-${WEEKDAYS[i].en}`);
    i += 1;
  }
  return parts.join(",");
}

/** A numeric field guard: clearing the input would parse as 0, junk as NaN. */
export function numericInput(raw: string): number | null {
  if (raw.trim() === "") return null;
  const parsed = Number(raw);
  return Number.isFinite(parsed) ? parsed : null;
}
