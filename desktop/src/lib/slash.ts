/**
 * The desktop slash-command catalog — the single source of truth for the popup,
 * the `/help` output, and `App`'s dispatch.
 *
 * Mirrors the "local" subset of the TUI's catalog
 * (`yi-agent-rs/crates/yi-agent/src/tui/slash.rs`). The TUI's 21 daemon-backed
 * commands (`/agents`, `/pause`, …) are deliberately absent: they reach the
 * daemon over a Unix socket, which the frontend never touches.
 */

export interface SlashCommandSpec {
  name: string;
  description: string;
  usage: string | null;
  needsArg: boolean;
}

export const SLASH_COMMANDS: SlashCommandSpec[] = [
  { name: "clear", description: "清空对话上下文", usage: null, needsArg: false },
  { name: "compact", description: "压缩对话历史", usage: null, needsArg: false },
  { name: "config", description: "显示当前配置", usage: null, needsArg: false },
  { name: "usage", description: "查看 token 用量与上下文占用", usage: null, needsArg: false },
  { name: "help", description: "显示帮助信息", usage: "[command]", needsArg: false },
];

/**
 * `/help` 卡片的分组——纯呈现层，不改变 `SLASH_COMMANDS` 的顺序（Popup 的顺序
 * 由它自己钉死）。
 *
 * 一致性由测试保证：拍平后必须恰好等于命令目录（不重、不漏、同序）。新增命令
 * 却忘了分组时，`slash.test.ts` 直接红。
 */
export const HELP_GROUPS: { title: string; commands: string[] }[] = [
  { title: "会话", commands: ["clear", "compact"] },
  { title: "状态", commands: ["config", "usage"] },
  { title: "帮助", commands: ["help"] },
];

/** Prefix filter, matching the TUI's `CommandPopup::filter` (not fuzzy). */
export function filterCommands(prefix: string): SlashCommandSpec[] {
  const p = prefix.trim();
  if (p === "") return SLASH_COMMANDS;
  return SLASH_COMMANDS.filter((c) => c.name.startsWith(p));
}

export type ParsedSlashInput =
  | { kind: "command"; name: string; args: string | null }
  | { kind: "path" }
  | { kind: "none" };

/**
 * Classify input that starts with `/`.
 *
 * A first token containing two or more slashes is an absolute path and belongs
 * to the agent, not the command popup — the same rule the TUI applies (see its
 * `submit_` / `unknown_slash_command_shows_error` tests): `/tmp` is a command
 * attempt, `/Users/me/src` is a path.
 */
export function parseSlashInput(text: string): ParsedSlashInput {
  const trimmed = text.trim();
  if (!trimmed.startsWith("/")) return { kind: "none" };
  const firstSpace = trimmed.search(/\s/);
  const firstToken = firstSpace === -1 ? trimmed : trimmed.slice(0, firstSpace);
  const slashes = [...firstToken].filter((ch) => ch === "/").length;
  if (slashes >= 2) return { kind: "path" };
  const name = firstToken.slice(1);
  if (name === "") return { kind: "none" };
  const rest = firstSpace === -1 ? "" : trimmed.slice(firstSpace).trim();
  return { kind: "command", name, args: rest === "" ? null : rest };
}
