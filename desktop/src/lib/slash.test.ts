import { describe, it, expect } from "vitest";
import {
  SLASH_COMMANDS,
  SLASH_ALIASES,
  filterCommands,
  parseSlashInput,
  HELP_GROUPS,
  canRunSlashDuringTurn,
  canonicalCommandName,
} from "./slash";

describe("slash command catalog", () => {
  it("lists exactly the supported commands, in popup order", () => {
    expect(SLASH_COMMANDS.map((c) => c.name)).toEqual([
      "clear",
      "compact",
      "fork",
      "config",
      "usage",
      "help",
      "side",
      "back",
      "side-close",
    ]);
  });

  it("does not list the /btw alias as its own catalog row", () => {
    // 别名进目录的话，帮助与补全里会出现两条一模一样的「打开旁问」。
    expect(SLASH_COMMANDS.map((c) => c.name)).not.toContain("btw");
    expect(SLASH_ALIASES.btw).toBe("side");
    expect(canonicalCommandName("btw")).toBe("side");
    expect(canonicalCommandName("side")).toBe("side");
    expect(canonicalCommandName("usage")).toBe("usage");
  });

  it("resolves /btw to the canonical /side row in the popup", () => {
    const hits = filterCommands("btw");
    // 别名折回规范名后按前缀匹配：`side` 与 `side-close` 都以它开头，两条都算
    // 合法候选（用户敲 /side 时自然该看到「关闭旁问」）。
    expect(hits.map((c) => c.name)).toEqual(["side", "side-close"]);
  });

  it("offers the side commands mid-turn so the parent is not interrupted", () => {
    for (const name of ["side", "back", "side-close"]) {
      expect(canRunSlashDuringTurn(name)).toBe(true);
    }
    // btw 是 side 的别名：解析后仍应在白名单里。
    expect(canRunSlashDuringTurn("btw")).toBe(true);
  });

  it("offers /fork with an optional message argument", () => {
    const fork = SLASH_COMMANDS.find((c) => c.name === "fork");
    expect(fork).toBeDefined();
    expect(fork!.usage).toBe("[message]");
    // 不强制带参数：不带就是「只建副本」。
    expect(fork!.needsArg).toBe(false);
    expect(fork!.description).toContain("会话空闲");
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

  it("names the groups in display order", () => {
    expect(HELP_GROUPS.map((g) => g.title)).toEqual([
      "会话",
      "旁问",
      "状态",
      "帮助",
    ]);
  });
});

// 去掉安全白名单或把未知命令默认放行时，这组测试必须失败。
describe("mid-turn slash policy", () => {
  it.each([
    ["help", true], ["usage", true], ["config", true],
    ["clear", false], ["compact", false], ["fork", false], ["nope", false],
  ])("classifies %s as available=%s", (name, available) => {
    expect(canRunSlashDuringTurn(name)).toBe(available);
  });
});
