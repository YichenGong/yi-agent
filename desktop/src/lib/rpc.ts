import type { ApprovalRequest, Decision, Notification, RequestId, RpcError } from "./protocol";

/** JSON-RPC error the server returns for a request that arrives before
 * `initialize` — `server not initialized`. The remote path recovers from this
 * automatically; see `RpcClient.request`. */
export const NOT_INITIALIZED = -32010;

/** Whether `error` is the server's `-32010 server not initialized`. */
function isNotInitialized(error: unknown): boolean {
  return (
    typeof error === "object" &&
    error !== null &&
    (error as { code?: unknown }).code === NOT_INITIALIZED
  );
}

/**
 * The raw request path, without auto-recovery. Handed to the recovery hook so
 * requests it issues during recovery cannot re-enter recovery.
 */
export type RequestFn = (method: string, params: unknown) => Promise<unknown>;

/** Abstracts the Tauri IPC boundary so the client is unit-testable. */
export interface Transport {
  send(message: { id: number; method: string; params: unknown }): Promise<void>;
  respond(id: string, result: Decision): Promise<void>;
  onMessage(cb: (message: unknown) => void): () => void;
  onRequest(cb: (request: ApprovalRequest) => void): () => void;
  onStatus(cb: (status: { state: string; code?: number | null }) => void): () => void;
  /**
   * Drop every event subscription this transport registered.
   *
   * The App builds a **new** transport on each reconnect, but a Tauri event
   * listener (unlike a closed WebSocket) keeps firing if it is never
   * unlistened. Without this the disposed client's transport would keep
   * delivering every frame, so an append-only `item/delta` would be applied once
   * per live client — the streamed text duplicates until `item/completed`
   * replaces the item wholesale ("显示重复内容，再收敛").
   */
  dispose(): void;
}

interface Pending {
  resolve: (value: unknown) => void;
  reject: (error: RpcError | Error) => void;
}

/**
 * Correlates JSON-RPC requests with their responses and fans notifications out
 * to subscribers.
 *
 * Request ids increase monotonically. `request()` registers its pending entry
 * synchronously before its first `await`, and a well-behaved server can only
 * respond after the request line has been written, so a response normally
 * always finds its pending entry. The buffer is retained as cheap defensive
 * insurance against protocol violations — unmatched, duplicate, or out-of-order
 * responses. A response with no pending entry is parked in `buffered` and
 * drained by the next request that allocates that id, so it is not lost.
 */
export class RpcClient {
  private nextId = 1;
  private pending = new Map<RequestId, Pending>();
  private buffered = new Map<RequestId, { result?: unknown; error?: RpcError }>();
  private notificationHandlers = new Set<(n: Notification) => void>();
  /**
   * The transport-level message subscription. Held so {@link dispose} can drop
   * it: a disconnected Tauri listener keeps firing, and a client replaced on
   * reconnect must stop feeding the UI from the old transport.
   */
  private unsubscribeTransport: () => void;
  private disposed = false;
  /**
   * The in-flight recovery handshake, at most one at a time. Several requests
   * can be refused with `-32010` in the same tick (a session switch fires
   * `thread/subscribe` and `thread/resume` together); without this they would
   * each run `initialize`, a handshake storm over one socket.
   */
  private recovery: Promise<void> | null = null;

  /**
   * `recover` is invoked when the server refuses a request with `-32010 server
   * not initialized`; the client then resends that request once. It receives a
   * **raw** request function (no auto-recovery) so the requests it issues while
   * recovering cannot re-enter recovery and wait on the very run they are part
   * of. See {@link request} for why this exists on the remote path.
   */
  constructor(
    private transport: Transport,
    private recover?: (raw: RequestFn) => Promise<void>,
  ) {
    this.unsubscribeTransport = transport.onMessage((raw) => this.onMessage(raw));
  }

  /**
   * Detach from the transport and stop delivering frames to subscribers.
   *
   * Call this before replacing a client on reconnect. Pending requests are
   * rejected so no caller is left awaiting a response from a transport that is
   * about to be abandoned. Idempotent: both the `exited` handler and `connect`
   * may dispose the same client.
   */
  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    this.unsubscribeTransport();
    this.notificationHandlers.clear();
    for (const [, pending] of this.pending) {
      pending.reject(new Error("rpc client disposed"));
    }
    this.pending.clear();
    this.buffered.clear();
    this.transport.dispose();
  }

  onNotification(handler: (n: Notification) => void): () => void {
    this.notificationHandlers.add(handler);
    return () => this.notificationHandlers.delete(handler);
  }

  onApproval(handler: (request: ApprovalRequest) => void): () => void {
    return this.transport.onRequest(handler);
  }

  onStatus(handler: (status: { state: string; code?: number | null }) => void): () => void {
    return this.transport.onStatus(handler);
  }

  async request<T = unknown>(method: string, params: unknown): Promise<T> {
    try {
      return await this.requestOnce<T>(method, params);
    } catch (error) {
      // 中继桥接重启后的自愈。经中继时全 session 只有**一条**电脑侧桥接连接被
      // 所有手机共享：app-server 换进程后桥接重连，服务端为这条新连接铸一个新的
      // `ws-<uuid>`（`initialized=false`），而手机的 socket 挂在中继上从未断开，
      // 也就不会重发 `initialize`。于是手机会在切 session 时被拒为 `-32010`
      // server not initialized——用户看到的就是「第一次连上了，切 session 就崩」。
      //
      // 在这里补一次「先握手、再重发本请求」：覆盖面最大（所有调用点自动受益），
      // 且不依赖用户把 App 切一次前后台来触发重连。
      if (
        !this.recover ||
        method === "initialize" || // 握手本身失败不能触发握手（会自递归）
        !isNotInitialized(error)
      ) {
        throw error;
      }
      await this.runRecovery();
      // 只重发一次：若重新握手后仍被拒为 `-32010`，就是真故障，照常抛出，
      // 不再循环（重试第二次不会比第一次更好，只会变成风暴）。
      return await this.requestOnce<T>(method, params);
    }
  }

  /** 一次请求往返，不含 `-32010` 自愈。 */
  private async requestOnce<T>(method: string, params: unknown): Promise<T> {
    const id = this.nextId++;
    const early = this.buffered.get(id);
    if (early) {
      // Response already arrived for this id: send, then consume it. The buffer
      // entry is only dropped after a successful send so a send failure does not
      // discard an already-received response.
      await this.transport.send({ id, method, params });
      this.buffered.delete(id);
      if (early.error) throw early.error;
      return early.result as T;
    }
    const promise = new Promise<T>((resolve, reject) => {
      this.pending.set(id, { resolve: resolve as (v: unknown) => void, reject });
    });
    try {
      await this.transport.send({ id, method, params });
    } catch (error) {
      // Don't leak the pending entry if the request never made it onto the wire.
      this.pending.delete(id);
      throw error;
    }
    return promise;
  }

  /**
   * Run the recovery handshake, coalescing concurrent callers onto one run.
   *
   * The promise is cleared before it resolves so a **later** refusal starts a
   * fresh handshake, while refusals that arrive during one run share it.
   */
  private runRecovery(): Promise<void> {
    if (!this.recovery) {
      const raw: RequestFn = (method, params) => this.requestOnce(method, params);
      this.recovery = this.recover!(raw).finally(() => {
        this.recovery = null;
      });
    }
    return this.recovery;
  }

  async respond(id: string, decision: Decision): Promise<void> {
    await this.transport.respond(id, decision);
  }

  private onMessage(raw: unknown): void {
    const msg = raw as {
      id?: RequestId;
      method?: string;
      params?: unknown;
      result?: unknown;
      error?: RpcError;
    };
    if (msg.id !== undefined && msg.method === undefined) {
      // Response.
      const pending = this.pending.get(msg.id);
      if (!pending) {
        this.buffered.set(msg.id, { result: msg.result, error: msg.error });
        return;
      }
      this.pending.delete(msg.id);
      if (msg.error) pending.reject(msg.error);
      else pending.resolve(msg.result);
      return;
    }
    if (msg.method !== undefined) {
      for (const handler of this.notificationHandlers) {
        handler(msg as unknown as Notification);
      }
    }
  }
}
