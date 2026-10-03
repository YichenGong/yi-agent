import { describe, it, expect } from "vitest";
import { detectMobile } from "./useIsMobile";

describe("detectMobile", () => {
  it("is true for a paired remote client", () => {
    expect(detectMobile(true, false)).toBe(true);
  });

  it("is true on iOS before a relay binding exists", () => {
    expect(detectMobile(false, true)).toBe(true);
  });

  it("is false on the desktop build", () => {
    expect(detectMobile(false, false)).toBe(false);
  });
});
