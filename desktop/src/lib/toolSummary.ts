/**
 * One-line summary of what a tool call is doing, for the collapsed card header.
 *
 * Mirrors the TUI's field choice (`yi-agent-rs/crates/yi-agent/src/tui/history.rs`
 * `permission_summary`): bash-like tools show the command, file tools show the
 * path. Anything unexpected returns null and the header falls back to the bare
 * tool name — we never stringify an arbitrary payload into the title.
 */

/** Tools whose most informative argument is `command`. */
const COMMAND_TOOLS = new Set(["bash", "process_start"]);
/** Tools whose most informative argument is `path`. */
const PATH_TOOLS = new Set(["read", "write", "edit", "view_image", "Skill"]);
/** Tools whose most informative argument is `pattern`. */
const PATTERN_TOOLS = new Set(["grep", "glob"]);
/** Tools whose most informative argument is `url`. */
const URL_TOOLS = new Set(["web_fetch"]);

const WEB_SEARCH = "web_search";
const MAX_COLUMNS = 80;

function pick(input: unknown, key: string): string | null {
  if (typeof input !== "object" || input === null || Array.isArray(input)) return null;
  const value = (input as Record<string, unknown>)[key];
  if (typeof value !== "string") return null;
  // First line only, with runs of whitespace collapsed: a multi-line heredoc
  // would otherwise render as mojibake in a single-line header.
  const oneLine = value.split("\n")[0].replace(/\s+/g, " ").trim();
  return oneLine === "" ? null : oneLine;
}

function truncate(text: string): string {
  if (text.length <= MAX_COLUMNS) return text;
  return `${text.slice(0, MAX_COLUMNS - 3)}...`;
}

export function toolCallSummary(name: string, input: unknown): string | null {
  let raw: string | null = null;
  if (COMMAND_TOOLS.has(name)) raw = pick(input, "command");
  else if (PATH_TOOLS.has(name)) raw = pick(input, "path");
  else if (PATTERN_TOOLS.has(name)) raw = pick(input, "pattern");
  else if (URL_TOOLS.has(name)) raw = pick(input, "url");
  else if (name === WEB_SEARCH) raw = pick(input, "query");
  return raw === null ? null : truncate(raw);
}
