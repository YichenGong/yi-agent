/**
 * 「连不上」这一类失败的统一识别与文案。
 *
 * 为什么需要它：RpcClient 在 dispose 时用 `new Error("rpc client disposed")` 拒绝
 * 所有在途请求（`lib/rpc.ts`），wsTransport 用 `websocket closed (1006)`，App 的
 * 各个接缝用 `not connected`，中继用 `no computer connected for this session`。
 * 这些字符串原样经 `formatError` 落到会话的 `lastError`，最终成为对话里一条红色
 * 横幅——用户看到的是一句英文，而且完全不知道该怎么办。
 *
 * 分层：
 * - **源头**抛结构化的 {@link ConnectionError}（message 仍是英文，日志与单测照旧
 *   可读），或是由线上来的、只有 code 的中继错误帧；
 * - **边界**（`formatError`）把两者都翻成一句可操作的中文；
 * - 重连成功后由 App 用 {@link clearConnectionErrorFrom} 收起这条提示——自动重连
 *   常常几秒就好，红条要是留着，反而像是一个还没解决的问题。
 */

/** 中继在没有电脑时回的错误码（`yi-agent-relay/src/lib.rs` 的 `NO_AGENT_ERROR_CODE`）。 */
export const NO_AGENT_ERROR_CODE = -32000;

/** 连接被换掉 / socket 断开 / 客户端还没建好：属于同一件事，等自动重连即可。 */
export const CONNECTION_DISCONNECTED_TEXT =
  "与电脑的连接已断开，正在自动重连，无需操作。";

/** 中继上没有电脑：手机侧等多久都不会好，需要人去开桌面端。 */
export const NO_COMPUTER_TEXT =
  "电脑端未连接。请确认桌面端 App 已打开，连上后会自动恢复。";

/** 连接失败的两类原因。 */
export type ConnectionErrorKind = "disconnected" | "no_computer";

/**
 * 客户端自己抛出的连接类错误。
 *
 * `message` 保持英文：它要能出现在日志、测试断言和 `title` 里；给人看的中文由
 * `formatError` 在边界处给出，两者不混在一起。
 */
export class ConnectionError extends Error {
  readonly kind: ConnectionErrorKind;
  constructor(kind: ConnectionErrorKind, message?: string) {
    super(message ?? (kind === "no_computer" ? "no computer connected" : "connection closed"));
    this.name = "ConnectionError";
    this.kind = kind;
  }
}

/** 这些 message 由客户端内部抛出，含义固定为「连接没了」。 */
const DISCONNECTED_MESSAGES = [
  "rpc client disposed",
  "not connected",
  // wsTransport 的两处 reject：`websocket closed (1006)` / `(unknown)`。
  "websocket closed",
];

/** 中继 no-agent 帧的原文；没有 code 时（老版本、或经别处转述）仍能认出来。 */
const NO_COMPUTER_MESSAGES = ["no computer connected"];

function messageOf(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (error && typeof error === "object" && "message" in error) {
    const m = (error as { message?: unknown }).message;
    if (typeof m === "string") return m;
  }
  return typeof error === "string" ? error : "";
}

/**
 * 判定一次失败是否属于「连不上」，返回具体原因；不是则 `null`。
 *
 * 结构化优先（我们自己的 `ConnectionError`、中继的 `-32000`），字面量匹配只作为
 * 退路——那种字符串是内部实现细节，改一处忘一处会漏，但只要它还在线上就必须认得。
 */
export function connectionErrorKind(error: unknown): ConnectionErrorKind | null {
  if (error instanceof ConnectionError) return error.kind;
  const code = (error as { code?: unknown } | null)?.code;
  if (code === NO_AGENT_ERROR_CODE) return "no_computer";
  const text = messageOf(error);
  if (NO_COMPUTER_MESSAGES.some((m) => text.includes(m))) return "no_computer";
  if (DISCONNECTED_MESSAGES.some((m) => text.includes(m))) return "disconnected";
  return null;
}

/** 把连接类失败翻成给人看的一句话；不是连接类则返回 `null`。 */
export function connectionErrorText(error: unknown): string | null {
  const kind = connectionErrorKind(error);
  if (kind === null) return null;
  return kind === "no_computer" ? NO_COMPUTER_TEXT : CONNECTION_DISCONNECTED_TEXT;
}

/** 这段文案是否是我们写进去的连接提示（重连成功时据此收起，不误伤业务错误）。 */
export function isConnectionMessage(text: string | null | undefined): boolean {
  return text === CONNECTION_DISCONNECTED_TEXT || text === NO_COMPUTER_TEXT;
}

/**
 * 重连成功后收起那条连接提示。返回是否真的改了（调用方据此决定要不要重渲染）。
 *
 * 只认 {@link isConnectionMessage} 的两句常量：业务错误（服务端拒绝、参数不对）
 * 不该因为「网又通了」而被抹掉——它跟连接无关，仍然要让人看见。
 */
export function clearConnectionErrorFrom(session: { lastError: string | null }): boolean {
  if (!isConnectionMessage(session.lastError)) return false;
  session.lastError = null;
  return true;
}
