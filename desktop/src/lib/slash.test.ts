import { describe, it, expect } from "vitest";
import {
  SLASH_COMMANDS,
  filterCommands,
  parseSlashInput,
  renderHelp,
} from "./slash";

describe("slash command catalog", () => {
  it("lists exactly the six supported commands, in popup order", () => {
    expect(SLASH_COMMANDS.map((c) => c.name)).toEqual([
      "clear",
      "compact",
      "config",
      "cost",
      "help",
      "model",
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
    expect(filterCommands("co").map((c) => c.name)).toEqual(["compact", "config", "cost"]);
    expect(filterCommands("cost").map((c) => c.name)).toEqual(["cost"]);
    expect(filterCommands("xyz")).toEqual([]);
  });

  it("returns every command for an empty prefix", () => {
    expect(filterCommands("")).toHaveLength(SLASH_COMMANDS.length);
  });
});

describe("parseSlashInput", () => {
  it("parses a bare command with no args", () => {
    expect(parseSlashInput("/cost")).toEqual({ kind: "command", name: "cost", args: null });
  });

  it("parses a command with args, trimmed", () => {
    expect(parseSlashInput("/help   cost  ")).toEqual({
      kind: "command",
      name: "help",
      args: "cost",
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

describe("renderHelp", () => {
  it("lists every command with usage and description", () => {
    const text = renderHelp();
    for (const c of SLASH_COMMANDS) {
      expect(text).toContain(`/${c.name}`);
      expect(text).toContain(c.description);
    }
  });

  it("describes one command when given a target", () => {
    expect(renderHelp("help")).toContain("用法");
  });

  it("reports an unknown command", () => {
    expect(renderHelp("nope")).toContain("未知命令");
  });
});
