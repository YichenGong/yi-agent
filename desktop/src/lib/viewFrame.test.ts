import { describe, it, expect } from "vitest";
import { resolveKind, parseLog } from "./viewFrame";

const frame = (over: Partial<Parameters<typeof resolveKind>[0]> = {}) => ({
  command: null,
  asKind: "text",
  exitCode: 0,
  stdout: "",
  stderr: "",
  truncated: false,
  timedOut: false,
  note: null,
  base: null,
  baseKind: "",
  ...over,
});

describe("resolveKind", () => {
  it("trusts a declared kind that matches the content", () => {
    expect(resolveKind(frame({ asKind: "diff", stdout: "diff --git a/x b/x\n" }))).toBe("diff");
  });

  it("falls back to text when the declared kind mismatches the content", () => {
    // 模型说是 diff，但内容不是：回退 text（保底永不失手）。
    expect(resolveKind(frame({ asKind: "diff", stdout: "just words\n" }))).toBe("text");
  });

  it("keeps the declared kind for text regardless of content", () => {
    expect(resolveKind(frame({ asKind: "text", stdout: "diff --git a/x b/x\n" }))).toBe("text");
  });
});

describe("parseLog", () => {
  it("splits oneline log entries", () => {
    expect(parseLog("abc1234 fix a thing\ndef5678 add another\n")).toEqual([
      { sha: "abc1234", subject: "fix a thing" },
      { sha: "def5678", subject: "add another" },
    ]);
  });

  it("tolerates a decorated log and empty input", () => {
    expect(parseLog("")).toEqual([]);
    expect(parseLog("no sha here\n")).toEqual([]);
  });
});
