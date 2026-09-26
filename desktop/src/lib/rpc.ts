import type { ApprovalRequest, Decision, Notification, RequestId, RpcError } from "./protocol";

/** Abstracts the Tauri IPC boundary so the client is unit-testable. */
export interface Transport {
  send(message: { id: number; method: string; params: unknown }): Promise<void>;
  respond(id: string, result: Decision): Promise<void>;
  onMessage(cb: (message: unknown) => void): () => void;
  onRequest(cb: (request: ApprovalRequest) => void): () => void;
  onStatus(cb: (status: { state: string; code?: number | null }) => void): () => void;
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

  constructor(private transport: Transport) {
    transport.onMessage((raw) => this.onMessage(raw));
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
