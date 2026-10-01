import { describe, expect, it } from "vitest";
import { formatError } from "./errorMessage";

describe("formatError", () => {
  it("prefers the message field of an object", () => {
    expect(formatError({ message: "unknown thread" })).toBe("unknown thread");
    expect(formatError({ code: -32011, message: "unknown thread" })).toBe("unknown thread");
  });

  it("falls back to String() for anything else", () => {
    expect(formatError("plain")).toBe("plain");
    expect(formatError(42)).toBe("42");
    // A non-string `message` is not a usable message, so String() on the object
    // wins - this mirrors the original App.tsx behaviour exactly.
    expect(formatError({ message: 7 })).toBe("[object Object]");
    expect(formatError(null)).toBe("null");
  });
});
