import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { ApprovalRequest } from "./lib/protocol";
import type { Transport } from "./lib/rpc";

/**
 * Subscribes to a bridge event and returns an unsubscribe function that is safe
 * to call before `listen` resolves. Without the `disposed` flag an early
 * unsubscribe would be lost: `listen` would later assign the real unlistener
 * into a closure that nobody calls, leaking the listener.
 */
function subscribe<T>(event: string, cb: (payload: T) => void): () => void {
  let unlisten: (() => void) | null = null;
  let disposed = false;
  void listen<T>(event, (e) => cb(e.payload))
    .then((u) => {
      if (disposed) u();
      else unlisten = u;
    })
    .catch((e) => console.error(`failed to listen on ${event}`, e));
  return () => {
    disposed = true;
    unlisten?.();
  };
}

/**
 * `Transport` backed by the Tauri bridge. Each hook subscribes to a bridge
 * event; `send`/`respond` invoke the two commands the Rust side exposes.
 */
export function tauriTransport(): Transport {
  // Every subscription must be unlistened on `dispose`. A Tauri event listener
  // (unlike a closed WebSocket) keeps firing forever if it is never removed, so
  // a client replaced on reconnect would otherwise keep applying every frame
  // alongside its successor.
  const unsubscribers = new Set<() => void>();
  const track = (off: () => void): (() => void) => {
    unsubscribers.add(off);
    return () => {
      unsubscribers.delete(off);
      off();
    };
  };
  return {
    send: async (message) => {
      await invoke("rpc", message);
    },
    respond: async (id, result) => {
      await invoke("rpc_respond", { id, result });
    },
    onMessage: (cb) => track(subscribe<unknown>("app-server://message", cb)),
    onRequest: (cb) => track(subscribe<ApprovalRequest>("app-server://request", cb)),
    onStatus: (cb) =>
      track(subscribe<{ state: string; code?: number | null }>("app-server://status", cb)),
    dispose: () => {
      for (const off of [...unsubscribers]) off();
      unsubscribers.clear();
    },
  };
}
