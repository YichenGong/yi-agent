/**
 * Turning a failed bridge call into something a person can act on.
 *
 * The plugin no longer shares the host's error module: what crosses the bridge
 * is an error `kind` from the protocol plus whatever detail string the host
 * chose. This file owns that mapping for the plugin, and nothing else does —
 * every catch site in the UI goes through `formatError` so the wording cannot
 * drift between panels.
 *
 * Connection loss is translated first and separately: "the socket went away,
 * retrying is pointless" and "the plugin is not installed" need different
 * actions from the user, and collapsing them into one message sends people to
 * click a button that will never succeed.
 */

/** The bridge's stable error kinds (protocol spec §6). */
export type BridgeErrorKind =
  | "Invalid"
  | "NotSupported"
  | "PermissionDenied"
  | "NotFound"
  | "Conflict"
  | "TooLarge"
  | "RateLimited"
  | "Busy"
  | "Timeout"
  | "StaleBinding"
  | "ResultUnknown"
  | "Internal";

/** What the board panel can usefully say about a failure. */
export type BoardErrorKind = "disconnected" | "plugin_missing" | "other";

const DISCONNECTED_TEXT = "与宿主的连接已断开，正在重连，无需操作。";
const PLUGIN_MISSING_TEXT = "看板插件未安装或未启用，请在插件设置中启用它。";

interface ErrorShape {
  kind?: unknown;
  detail?: unknown;
  message?: unknown;
  data?: { code?: unknown };
}

function shapeOf(error: unknown): ErrorShape | null {
  return typeof error === "object" && error !== null ? (error as ErrorShape) : null;
}

function textOf(error: unknown): string {
  const shape = shapeOf(error);
  if (typeof shape?.detail === "string") return shape.detail;
  if (typeof shape?.message === "string") return shape.message;
  if (error instanceof Error) return error.message;
  return String(error ?? "");
}

/**
 * Whether the failure means "the bridge itself is gone" rather than "the
 * request was refused". A refused request is worth showing verbatim; a dead
 * bridge is not the user's problem to diagnose.
 */
export function errorKind(error: unknown): BoardErrorKind {
  const shape = shapeOf(error);
  const kind = typeof shape?.kind === "string" ? shape.kind : "";
  if (kind === "StaleBinding" || kind === "ResultUnknown") return "other";

  const code = typeof shape?.data?.code === "string" ? shape.data.code : "";
  if (code === "plugin_unavailable" || code === "plugin_not_installed") {
    return "plugin_missing";
  }

  const text = textOf(error);
  // The host closes the port on unload and rejects in-flight frames; both read
  // as a disconnect, and retrying before the host re-handshakes would only
  // produce the same rejection.
  if (
    text.includes("not connected") ||
    text.includes("websocket closed") ||
    text.includes("bridge closed") ||
    text.includes("disposed")
  ) {
    return "disconnected";
  }
  if (text.includes("is not installed") || text.includes("plugin_unavailable")) {
    return "plugin_missing";
  }
  return "other";
}

/** Render a failure for a person. Prefers the host's own detail when we have one. */
export function formatError(error: unknown): string {
  const kind = errorKind(error);
  if (kind === "disconnected") return DISCONNECTED_TEXT;
  if (kind === "plugin_missing") return PLUGIN_MISSING_TEXT;
  const text = textOf(error);
  return text === "" ? "操作失败，请重试。" : text;
}

/** A `ResultUnknown` write is not a failure: the host may have committed it. */
export function isResultUnknown(error: unknown): boolean {
  const shape = shapeOf(error);
  return shape?.kind === "ResultUnknown";
}

/** Outcome of a write whose fate we could not determine. */
export const UNKNOWN_WRITE_TEXT =
  "结果未知：这次操作可能已经生效，请刷新核对后再决定是否重试。";
