import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { ApprovalRequest } from "./lib/protocol";
import type { Transport } from "./lib/rpc";

/**
 * `Transport` backed by the Tauri bridge. Each hook subscribes to a bridge
 * event; `send`/`respond` invoke the two commands the Rust side exposes.
 */
export function tauriTransport(): Transport {
  return {
    send: async (message) => {
      await invoke("rpc", message);
    },
    respond: async (id, result) => {
      await invoke("rpc_respond", { id, result });
    },
    onMessage: (cb) => {
      let unlisten = () => {};
      void listen<unknown>("app-server://message", (e) => cb(e.payload)).then((u) => (unlisten = u));
      return () => unlisten();
    },
    onRequest: (cb) => {
      let unlisten = () => {};
      void listen<ApprovalRequest>("app-server://request", (e) => cb(e.payload)).then(
        (u) => (unlisten = u),
      );
      return () => unlisten();
    },
    onStatus: (cb) => {
      let unlisten = () => {};
      void listen<{ state: string; code?: number | null }>("app-server://status", (e) =>
        cb(e.payload),
      ).then((u) => (unlisten = u));
      return () => unlisten();
    },
  };
}
