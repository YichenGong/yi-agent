/**
 * 二维码配对载荷编解码（S4）。
 *
 * 契约与 Rust 侧 `yi-agent-app-server/src/pair_uri.rs` 完全一致：
 * `yiagent://pair?v=1&relay=<percent-encoded relay url>&code=<code>`。
 * 两份实现用同一组 fixture 断言，防止格式漂移。纯函数、无依赖。
 *
 * `relay` 可装中继地址或局域网直连地址；因自身含 `?`，作 query 值须 form-encode。
 */

export const PAIR_SCHEME = "yiagent";
export const PAIR_HOST = "pair";
export const PAIR_VERSION = "1";

/** 按契约拼出二维码文本（form 编码，与 Rust `form_urlencoded` 一致）。 */
export function buildPairUri(relay: string, code: string): string {
  const p = new URLSearchParams();
  p.set("v", PAIR_VERSION);
  p.set("relay", relay);
  p.set("code", code);
  return `${PAIR_SCHEME}://${PAIR_HOST}?${p.toString()}`;
}

/** 解析二维码文本；不满足契约（scheme/host/版本/字段/ws 前缀任一不符）返回 null。 */
export function parsePairUri(text: string): { relay: string; code: string } | null {
  let u: URL;
  try {
    u = new URL(text);
  } catch {
    return null;
  }
  if (u.protocol !== `${PAIR_SCHEME}:` || u.hostname !== PAIR_HOST) return null;
  if (u.searchParams.get("v") !== PAIR_VERSION) return null;
  const relay = u.searchParams.get("relay") ?? "";
  const code = u.searchParams.get("code") ?? "";
  if (relay.length === 0 || code.length === 0) return null;
  if (!(relay.startsWith("ws://") || relay.startsWith("wss://"))) return null;
  return { relay, code };
}

/**
 * The phone-side relay endpoint derived from the desktop-side one.
 * `/connect` → `/ws` (same scheme/host/port/query). Returns null when the input
 * cannot be parsed or its path does not end in `connect` (caller then falls back).
 */
export function phoneRelayUrl(desktopUrl: string): string | null {
  let u: URL;
  try {
    u = new URL(desktopUrl);
  } catch {
    return null;
  }
  // 中继的两个出站路由是**不同**的：电脑连 `/connect`，手机连 `/ws`。
  // 只替换末段路径，scheme/host/port/query 原样保留。
  const segments = u.pathname.split("/");
  if (segments[segments.length - 1] !== "connect") return null;
  segments[segments.length - 1] = "ws";
  u.pathname = segments.join("/");
  return u.toString();
}
