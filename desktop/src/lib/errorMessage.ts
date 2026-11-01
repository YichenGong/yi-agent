import { connectionErrorText } from "./connectionError";

/**
 * Turn an unknown thrown value into something a person can read.
 *
 * RPC rejections are `RpcError` objects, so `String(e)` would render
 * `[object Object]`. Prefer the `message` field when present, falling back to
 * the default coercion for primitives and other shapes.
 *
 * **连接类失败先翻译**：`rpc client disposed` / `websocket closed (1006)` /
 * `not connected` / 中继的 `no computer connected for this session` 都是内部
 * 实现细节，原样露给用户既看不懂也不知道该做什么。它们在这一个出口统一换成
 * 一句可操作的中文（见 `connectionError.ts`），因此所有 20 多个 `catch` 调用点
 * ——对话横幅、设置页、diff 面板、图片、看板——自动受益，不必逐个改。
 *
 * Lives here rather than in `App.tsx` because the board panel's error paths need
 * the same rendering as the rest of the shell.
 */
export function formatError(e: unknown): string {
  const connection = connectionErrorText(e);
  if (connection !== null) return connection;
  if (e && typeof e === "object" && "message" in e) {
    const m = (e as { message?: unknown }).message;
    if (typeof m === "string") return m;
  }
  return String(e);
}
