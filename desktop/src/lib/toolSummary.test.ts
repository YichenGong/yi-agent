import { describe, it, expect } from "vitest";
import { toolCallSummary } from "./toolSummary";

describe("toolCallSummary", () => {
  it("uses the command for bash and process_start", () => {
    expect(toolCallSummary("bash", { command: "git status" })).toBe("git status");
    expect(toolCallSummary("process_start", { command: "npm run dev" })).toBe("npm run dev");
  });

  it("uses the path for file tools", () => {
    expect(toolCallSummary("read", { path: "src/App.tsx" })).toBe("src/App.tsx");
    expect(toolCallSummary("write", { path: "docs/x.md" })).toBe("docs/x.md");
    expect(toolCallSummary("edit", { path: "a.rs" })).toBe("a.rs");
    expect(toolCallSummary("view_image", { path: "/tmp/a.png" })).toBe("/tmp/a.png");
  });

  it("uses the pattern for grep and glob", () => {
    expect(toolCallSummary("grep", { pattern: "rename", path: "desktop" })).toBe("rename");
    expect(toolCallSummary("glob", { pattern: "**/*.tsx" })).toBe("**/*.tsx");
  });

  it("uses the path for the Skill tool", () => {
    expect(toolCallSummary("Skill", { path: "/home/u/.yi-agent/skills/x/SKILL.md" })).toBe(
      "/home/u/.yi-agent/skills/x/SKILL.md",
    );
  });

  it("uses the query and url for web tools", () => {
    expect(toolCallSummary("web_search", { query: "rust lifetimes" })).toBe("rust lifetimes");
    expect(toolCallSummary("web_fetch", { url: "https://example.com" })).toBe(
      "https://example.com",
    );
  });

  it("returns null when the field is absent", () => {
    expect(toolCallSummary("bash", {})).toBeNull();
    expect(toolCallSummary("read", { offset: 1 })).toBeNull();
    expect(toolCallSummary("bash", { command: 42 })).toBeNull();
    expect(toolCallSummary("bash", { command: "" })).toBeNull();
    expect(toolCallSummary("bash", { command: "   " })).toBeNull();
  });

  it("returns null for unknown tools and non-object input", () => {
    expect(toolCallSummary("mystery", { command: "x" })).toBeNull();
    expect(toolCallSummary("bash", null)).toBeNull();
    expect(toolCallSummary("bash", "git status")).toBeNull();
    expect(toolCallSummary("bash", 42)).toBeNull();
    expect(toolCallSummary("bash", ["git status"])).toBeNull();
  });

  it("collapses whitespace and keeps only the first line", () => {
    expect(toolCallSummary("bash", { command: "mkdir -p a\ncd a\ntouch b" })).toBe("mkdir -p a");
    expect(toolCallSummary("bash", { command: "  ls   -la  " })).toBe("ls -la");
  });

  it("truncates to 80 columns with an ellipsis", () => {
    const long = "x".repeat(200);
    const out = toolCallSummary("bash", { command: long });
    expect(out).not.toBeNull();
    expect(out!.endsWith("...")).toBe(true);
    expect(out!.length).toBeLessThanOrEqual(80);
    const exact = "y".repeat(80);
    expect(toolCallSummary("bash", { command: exact })).toBe(exact);
  });
});
