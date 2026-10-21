import { describe, it, expect } from "vitest";
import {
  SLASH_COMMANDS,
  filterCommands,
  parseSlashInput,
  HELP_GROUPS,
} from "./slash";

describe("slash command catalog", () => {
  it("lists exactly the five supported commands, in popup order", () => {
    expect(SLASH_COMMANDS.map((c) => c.name)).toEqual([
      "clear",
      "compact",
      "config",
      "usage",
      "help",
    ]);
  });

  it("does not offer /quit (closing the window is a system action)", () => {
    expect(SLASH_COMMANDS.find((c) => c.name === "quit")).toBeUndefined();
  });

  it("gives every command a non-empty Chinese description", () => {
    for (const c of SLASH_COMMANDS) {
      expect(c.description.length).toBeGreaterThan(0);
    }
  });

  it("filters by prefix, not fuzzy match", () => {
    expect(filterCommands("co").map((c) => c.name)).toEqual(["compact", "config"]);
    expect(filterCommands("us").map((c) => c.name)).toEqual(["usage"]);
    expect(filterCommands("xyz")).toEqual([]);
  });

  it("returns every command for an empty prefix", () => {
    expect(filterCommands("")).toHaveLength(SLASH_COMMANDS.length);
  });
});

describe("parseSlashInput", () => {
  it("parses a bare command with no args", () => {
    expect(parseSlashInput("/usage")).toEqual({ kind: "command", name: "usage", args: null });
  });

  it("parses a command with args, trimmed", () => {
    expect(parseSlashInput("/help   usage  ")).toEqual({
      kind: "command",
      name: "help",
      args: "usage",
    });
  });

  it("leaves plain text alone", () => {
    expect(parseSlashInput("hello")).toEqual({ kind: "none" });
  });

  it("treats a first token with two or more slashes as a path", () => {
    // TUI parity: /Users/me/src is a path, not the /Users command.
    expect(parseSlashInput("/Users/me/src")).toEqual({ kind: "path" });
    expect(parseSlashInput("/tmp/some/dir explain this")).toEqual({ kind: "path" });
  });

  it("still treats a single-slash token as a command", () => {
    expect(parseSlashInput("/tmp")).toEqual({ kind: "command", name: "tmp", args: null });
  });
});

describe("help groups", () => {
  it("flattens to exactly the command catalog, with no duplicates", () => {
    // 分组是呈现层，允许按用途重排，但每条命令必须恰好出现一次：不漏、不重。
    // 新增命令却忘了分组时，这条测试直接红。
    const flat = HELP_GROUPS.flatMap((g) => g.commands);
    expect(new Set(flat)).toEqual(new Set(SLASH_COMMANDS.map((c) => c.name)));
    expect(flat).toHaveLength(SLASH_COMMANDS.length);
  });

  it("names the three groups", () => {
    expect(HELP_GROUPS.map((g) => g.title)).toEqual(["会话", "状态", "帮助"]);
  });
});
