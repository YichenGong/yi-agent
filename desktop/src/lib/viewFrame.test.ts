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

  it("trusts a declared log only when the content is a log", () => {
    // 声明 log + 内容确为 log -> log；声明 log + 内容不是 log -> text。
    // 这两条覆盖 `case "log"` 分支：把它写成无条件 return "log" 或对调两支，
    // 之前的三条测试仍会全绿。
    expect(resolveKind(frame({ asKind: "log", stdout: "abc1234 fix a\n" }))).toBe("log");
    expect(resolveKind(frame({ asKind: "log", stdout: "just words\n" }))).toBe("text");
  });

  it("degrades an unknown or empty asKind to text", () => {
    // `default` 分支：不在三值里的声明不猜，直接纯文本。
    expect(resolveKind(frame({ asKind: "svg", stdout: "diff --git a/x b/x\n" }))).toBe("text");
    expect(resolveKind(frame({ asKind: "", stdout: "abc1234 fix a\n" }))).toBe("text");
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

  it("parses CRLF oneline output", () => {
    // Windows 检出 / 经由某种转换后可能是 \r\n：`.` 不匹配 `\r`，按 "\n" 切会让每行
    // 都因行尾的 \r 匹配不上正则而整行被丢，面板空白。
    expect(parseLog("abc1234 fix a\r\ndef5678 add b\r\n")).toEqual([
      { sha: "abc1234", subject: "fix a" },
      { sha: "def5678", subject: "add b" },
    ]);
  });

  it("accepts uppercase hex, like the backend's is_ascii_hexdigit", () => {
    // 后端 `classify` 用 `is_ascii_hexdigit`（收 A-F）；前端若不收，大写 sha 的 log
    // 会被 `resolveKind` 降级成纯文本。
    expect(resolveKind(frame({ asKind: "log", stdout: "ABC1234 fix a\n" }))).toBe("log");
    expect(parseLog("ABC1234 fix a\n")).toEqual([{ sha: "ABC1234", subject: "fix a" }]);
  });
});
