import { describe, it, expect } from "vitest";
import { threadStartParams } from "./threadStart";

describe("threadStartParams", () => {
  it("omits cwd when not provided", () => {
    expect(threadStartParams()).toEqual({});
  });

  it("includes cwd when provided", () => {
    expect(threadStartParams("/tmp/x")).toEqual({ cwd: "/tmp/x" });
  });
});
