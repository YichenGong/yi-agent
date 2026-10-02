import type { ApprovalRequest } from "./lib/protocol";
import type { Transport } from "./lib/rpc";

/** Minimal shape of the browser `WebSocket` this transport relies on. */
export type WebSocketFactory = (url: string) => WebSocket;

/** Close code the server uses when the handshake carried no/invalid token. */
export const UNAUTHORIZED_CLOSE_CODE = 4401;

/**
 * Append the device token as a `?token=` query parameter.
 *
 * The relay authenticates at the *WebSocket upgrade*: `Authorization: Bearer`
 * is checked first, then `?token=`. A browser — hence the iOS WKWebView — cannot
 * set request headers on a `WebSocket`, so the query form is the only one
 * available to the app; the tests pin this choice. (The plan's Step 3 sketch
 * showed an `auth` JSON frame sent after open; the committed server does not
 * accept that, so this module deliberately does not implement it. See the task
 * report "deviation from the plan".)
 */
export function withToken(url: string, token: string): string {
  const u = new URL(url);
  u.searchParams.set("token", token);
  return u.toString();
}

/**
 * `Transport` backed by a plain WebSocket — the remote client's half of the
 * app-server protocol.
 *
 * It deliberately assumes nothing about Tauri: it is a browser `WebSocket` the
 * iOS webview can run. `factory` is the seam that keeps it unit-testable
 * without a live socket; it defaults to the global `WebSocket`.
 *
 * Frames are classified the way `RpcClient` expects:
 * - `{id, result|error}` (no `method`) → a response → `onMessage`;
 * - `{method, id}` → a reverse request (tool approval) → `onRequest`;
 * - `{method}` (no `id`) → a notification → `onMessage`.
 */
export function wsTransport(
  url: string,
  token: string,
  factory: WebSocketFactory = (u) => new WebSocket(u),
): Transport {
  const socket = factory(withToken(url, token));
  const messageHandlers = new Set<(message: unknown) => void>();
  const requestHandlers = new Set<(request: ApprovalRequest) => void>();
  const statusHandlers = new Set<(status: { state: string; code?: number | null }) => void>();

  // A real socket throws if written while CONNECTING, so `send`/`respond` wait
  // for the open. If the socket closes first the waiters reject, which keeps
  // `RpcClient` from leaving a request pending forever.
  let opened = false;
  let closed = false;
  let closeCode: number | null = null;
  const waiters: Array<{ resolve: () => void; reject: (e: Error) => void }> = [];

  const ready = (): Promise<void> => {
    if (closed) {
      return Promise.reject(new Error(`websocket closed (${closeCode ?? "unknown"})`));
    }
    if (opened) return Promise.resolve();
    return new Promise<void>((resolve, reject) => waiters.push({ resolve, reject }));
  };
  const settle = (fn: (w: (typeof waiters)[number]) => void) => {
    for (const w of waiters.splice(0)) fn(w);
  };
  const write = async (frame: unknown): Promise<void> => {
    await ready();
    socket.send(JSON.stringify(frame));
  };

  socket.onopen = () => {
    opened = true;
    settle((w) => w.resolve());
  };
  socket.onmessage = (e) => {
    let v: unknown;
    try {
      v = JSON.parse(typeof e.data === "string" ? e.data : String(e.data));
    } catch {
      return; // A non-JSON frame is not part of the protocol; drop it.
    }
    if (typeof v !== "object" || v === null) return;
    const frame = v as { id?: unknown; method?: string };
    if (frame.method !== undefined && frame.id !== undefined) {
      for (const h of requestHandlers) h(v as ApprovalRequest);
    } else {
      for (const h of messageHandlers) h(v);
    }
  };
  socket.onclose = (e) => {
    closed = true;
    closeCode = e.code ?? null;
    settle((w) => w.reject(new Error(`websocket closed (${closeCode ?? "unknown"})`)));
    for (const h of statusHandlers) h({ state: "exited", code: closeCode });
  };

  return {
    send: async (message) => write({ jsonrpc: "2.0", ...message }),
    respond: async (id, result) => write({ jsonrpc: "2.0", id, result }),
    onMessage: (cb) => {
      messageHandlers.add(cb);
      return () => messageHandlers.delete(cb);
    },
    onRequest: (cb) => {
      requestHandlers.add(cb);
      return () => requestHandlers.delete(cb);
    },
    onStatus: (cb) => {
      statusHandlers.add(cb);
      return () => statusHandlers.delete(cb);
    },
  };
}
