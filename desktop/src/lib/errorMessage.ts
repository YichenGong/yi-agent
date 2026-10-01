/**
 * Turn an unknown thrown value into something a person can read.
 *
 * RPC rejections are `RpcError` objects, so `String(e)` would render
 * `[object Object]`. Prefer the `message` field when present, falling back to
 * the default coercion for primitives and other shapes.
 *
 * Lives here rather than in `App.tsx` because the board panel's error paths need
 * the same rendering as the rest of the shell.
 */
export function formatError(e: unknown): string {
  if (e && typeof e === "object" && "message" in e) {
    const m = (e as { message?: unknown }).message;
    if (typeof m === "string") return m;
  }
  return String(e);
}
