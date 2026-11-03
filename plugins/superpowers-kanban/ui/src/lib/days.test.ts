import { describe, expect, it } from "vitest";
import { WEEKDAYS, numericInput, parseDays, renderDays } from "./days";

describe("weekday specs", () => {
  it("parses a range, a list and a single day, Monday first", () => {
    expect(parseDays("Mon-Fri")).toEqual([true, true, true, true, true, false, false]);
    expect(parseDays("Sat,Sun")).toEqual([false, false, false, false, false, true, true]);
    expect(parseDays("Wed")).toEqual([false, false, true, false, false, false, false]);
  });

  it("accepts full names, any case, and stray spaces", () => {
    expect(parseDays("  monday - friday ")).toEqual([
      true,
      true,
      true,
      true,
      true,
      false,
      false,
    ]);
    expect(parseDays("SUN")).toEqual([false, false, false, false, false, false, true]);
  });

  it("reads nothing selected, empty segments and unknown names as unselected", () => {
    expect(parseDays("")).toEqual(Array(7).fill(false));
    expect(parseDays(",,")).toEqual(Array(7).fill(false));
    expect(parseDays("Funday")).toEqual(Array(7).fill(false));
  });

  it("ignores a reversed range instead of selecting everything", () => {
    expect(parseDays("Fri-Mon")).toEqual(Array(7).fill(false));
  });

  it("compresses a selection back into the canonical spec", () => {
    expect(renderDays([true, true, true, true, true, false, false])).toBe("Mon-Fri");
    // Saturday and Sunday are adjacent, so they compress to a range just like
    // any other run — matching the backend's own `render_days`, not "Sat,Sun".
    expect(renderDays([false, false, false, false, false, true, true])).toBe("Sat-Sun");
    expect(renderDays([true, false, true, false, true, false, false])).toBe("Mon,Wed,Fri");
    expect(renderDays(Array(7).fill(true))).toBe("Mon-Sun");
    expect(renderDays(Array(7).fill(false))).toBe("");
  });

  it("round-trips every canonical spec the backend produces", () => {
    for (const spec of ["Mon-Fri", "Sat-Sun", "Mon,Wed,Fri", "Sun", "Mon-Sun"]) {
      expect(renderDays(parseDays(spec))).toBe(spec);
    }
    // A hand-written comma form reads fine but is written back canonically:
    // the backend is the authority on the stored form, and the UI must not
    // preserve a shape the backend would immediately rewrite.
    expect(renderDays(parseDays("Sat,Sun"))).toBe("Sat-Sun");
  });

  it("keeps one canonical label per weekday", () => {
    expect(WEEKDAYS.map((d) => d.en)).toEqual([
      "Mon",
      "Tue",
      "Wed",
      "Thu",
      "Fri",
      "Sat",
      "Sun",
    ]);
  });
});

describe("numeric input guard", () => {
  it("returns null for an empty or unparsable value", () => {
    expect(numericInput("")).toBeNull();
    expect(numericInput("   ")).toBeNull();
    expect(numericInput("abc")).toBeNull();
  });

  it("returns the number for a real value", () => {
    expect(numericInput("3")).toBe(3);
    expect(numericInput(" 0 ")).toBe(0);
  });
});
