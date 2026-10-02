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

/**
 * Redeem a one-time code over a **normal session**, as a JSON-RPC request
 * (`pair/redeem`) rather than the `?pair=` upgrade query.
 *
 * This is the path that works **through the relay**: the relay forwards ws
 * frames but does not rewrite the upgrade query string, so a phone's `?pair=`
 * never reaches the local app-server. Once the app has any session open (the
 * relay bridge holds one), the frame is forwarded verbatim and the server mints
 * the device there. `pair/redeem` is deliberately **not** admin-gated — the
 * one-time code is the credential.
 *
 * Resolves with the minted device; rejects with an error whose `code` is the
 * JSON-RPC error code (`-32001` for a bad/used/expired code) so callers can
 * distinguish it from a transport failure.
 */
export function redeemPairCodeViaRpc(
  send: (method: string, params: unknown) => Promise<unknown>,
  code: string,
  deviceName: string,
): Promise<PairedDevice> {
  return send("pair/redeem", { code, device_name: deviceName }).then((result) => {
    const r = result as { device_id?: unknown; token?: unknown; scope?: unknown } | null;
    if (!r || typeof r.token !== "string" || typeof r.device_id !== "string") {
      throw new Error("pair/redeem returned an unexpected payload");
    }
    const scope = typeof r.scope === "string" ? r.scope : "control";
    return { device_id: r.device_id, token: r.token, scope };
  });
}

/** True when `url` points at the relay's app endpoint (`/ws?session=…`). */
export function isRelayUrl(url: string): boolean {
  try {
    return new URL(url).searchParams.has("session");
  } catch {
    return false;
  }
}

/**
 * The default redeem for the pairing screen: pick the transport that can
 * actually reach the app-server.
 *
 * - **Relay URL** (`?session=…`): open a tokenless ws to the relay and redeem
 *   over frames (`initialize` → `pair/redeem`). The relay forwards frames but
 *   not the upgrade query string, so `?pair=` cannot work here; and the relay
 *   does not gate on a token, so a tokenless channel is fine for this one-shot
 *   exchange. This is the WAN path.
 * - **Direct app-server URL** (`ws://host:port/ws`): the server rejects a
 *   tokenless connection (4401) before it can carry frames, so use the
 *   `?pair=` upgrade-query form.
 */
export function defaultRedeem(
  url: string,
  code: string,
  deviceName: string,
  factory: WebSocketFactory = (u) => new WebSocket(u),
): Promise<PairedDevice> {
  if (!isRelayUrl(url)) return redeemPairCode(url, code, deviceName, factory);
  return redeemPairCodeViaRelay(url, code, deviceName, factory);
}

/**
 * Redeem over the relay by opening a tokenless ws and speaking JSON-RPC frames.
 * Resolves on the `pair/redeem` result; rejects on a JSON-RPC error (with the
 * error code attached) or a socket failure.
 */
export function redeemPairCodeViaRelay(
  url: string,
  code: string,
  deviceName: string,
  factory: WebSocketFactory = (u) => new WebSocket(u),
): Promise<PairedDevice> {
  const socket = factory(url);
  return new Promise<PairedDevice>((resolve, reject) => {
    let settled = false;
    const finish = (fn: () => void) => {
      if (settled) return;
      settled = true;
      fn();
      try {
        socket.close();
      } catch {
        /* ignore */
      }
    };
    let inited = false;
    const send = (id: number, method: string, params: unknown) => {
      socket.send(JSON.stringify({ jsonrpc: "2.0", id, method, params }));
    };

    socket.onopen = () => send(1, "initialize", { clientInfo: { name: "yi-agent-ios" } });
    socket.onmessage = (e) => {
      let v: { id?: unknown; method?: string; result?: unknown; error?: { code?: number; message?: string } };
      try {
        v = JSON.parse(typeof e.data === "string" ? e.data : String(e.data));
      } catch {
        return;
      }
      if (v.id === 1 && !inited) {
        inited = true;
        send(2, "pair/redeem", { code, device_name: deviceName });
        return;
      }
      if (v.id === 2) {
        if (v.error) {
          const err = new Error(v.error.message ?? "pair/redeem failed") as Error & { code?: number };
          err.code = v.error.code;
          finish(() => reject(err));
          return;
        }
        const r = (v.result ?? {}) as Partial<PairedDevice>;
        if (typeof r.token !== "string" || typeof r.device_id !== "string") {
          finish(() => reject(new Error("pair/redeem returned an unexpected payload")));
          return;
        }
        const device_id = r.device_id;
        const token = r.token;
        const scope = r.scope ?? "control";
        finish(() => resolve({ device_id, token, scope }));
      }
    };
    socket.onclose = (e) => {
      if (!settled) {
        const code_ = e.code;
        finish(() =>
          reject(
            code_ === 4401
              ? new Error("pairing rejected (4401)")
              : new Error(`pairing socket closed (${code_ ?? "unknown"})`),
          ),
        );
      }
    };
    socket.onerror = () => finish(() => reject(new Error("pairing socket error")));
  });
}
