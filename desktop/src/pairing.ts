/**
 * First-launch pairing: redeem a one-time code (shown as a QR code by the
 * desktop) for a device token, over the relay.
 *
 * Server contract (`yi-agent-rs/.../ws.rs`, `redeem_pair_code`): connect to
 * `ws(s)://host/ws?pair=<code>&device_name=<name>`; the server replies with one
 * Text frame
 *
 *   {"jsonrpc":"2.0","method":"pair/redeemed",
 *    "params":{"device_id":"dev-…","token":"yia_…","scope":"control"}}
 *
 * and then closes with code **4403** ("paired"). A bad/used/expired code is
 * indistinguishable from "no token": the server closes with **4401**.
 *
 * The delivered connection is not a session — it is closed immediately — so the
 * caller must persist the token and reconnect normally (see `wsTransport`).
 */

/** Close code the server uses to signal "pairing delivered, reconnect with token". */
export const PAIRING_DELIVERED_CLOSE_CODE = 4403;

export interface PairedDevice {
  device_id: string;
  token: string;
  scope: string;
}

export type WebSocketFactory = (url: string) => WebSocket;

/** Build the redeem url. Exported so callers/tests can inspect the handshake. */
export function pairUrl(url: string, code: string, deviceName: string): string {
  const u = new URL(url);
  u.searchParams.set("pair", code);
  u.searchParams.set("device_name", deviceName);
  return u.toString();
}

/**
 * Perform the pairing handshake and resolve the minted device credentials.
 *
 * Resolves on the `pair/redeemed` frame without waiting for the trailing 4403
 * close. Rejects if the socket closes before a frame arrives (4401 for a bad
 * code), or on a frame that is not a well-formed `pair/redeemed` payload.
 */
export function redeemPairCode(
  url: string,
  code: string,
  deviceName: string,
  factory: WebSocketFactory = (u) => new WebSocket(u),
): Promise<PairedDevice> {
  const socket = factory(pairUrl(url, code, deviceName));

  return new Promise<PairedDevice>((resolve, reject) => {
    let settled = false;
    const finish = (fn: () => void) => {
      if (settled) return;
      settled = true;
      socket.onmessage = null;
      socket.onclose = null;
      fn();
    };

    socket.onmessage = (e) => {
      let v: { method?: string; params?: Partial<PairedDevice> };
      try {
        v = JSON.parse(typeof e.data === "string" ? e.data : String(e.data));
      } catch {
        return; // Ignore anything that is not the delivered frame.
      }
      if (v?.method !== "pair/redeemed" || !v.params) return;
      const { device_id, token, scope } = v.params;
      if (typeof device_id !== "string" || typeof token !== "string") return;
      finish(() => resolve({ device_id, token, scope: scope ?? "control" }));
    };
    socket.onclose = (e) => {
      const code_ = e.code;
      // 4403 here means "delivered then closed" — only reachable if the frame
      // was missed; either way there is no token to hand back.
      if (code_ === PAIRING_DELIVERED_CLOSE_CODE) {
        finish(() => reject(new Error("pairing closed (4403) before pair/redeemed arrived")));
      } else if (code_ === 4401) {
        finish(() => reject(new Error("pairing rejected: invalid or used code (4401)")));
      } else {
        finish(() => reject(new Error(`pairing socket closed (${code_ ?? "unknown"})`)));
      }
    };
    socket.onerror = () => finish(() => reject(new Error("pairing socket error")));
  });
}
