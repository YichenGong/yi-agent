/**
 * §1.1 / §8.1: per-frame wire overhead for `item/delta`.
 *
 * Background data for the funnel item (remote delta coalescing) that this spec
 * deliberately does **not** do. It shows why frame *count* — not frame *size* —
 * is what would matter there: every frame carries a fixed JSON-RPC envelope, so
 * a one-byte delta costs ~162 bytes on the wire.
 *
 * The envelope below reproduces the real shape from
 * `yi-agent-app-server/src/protocol.rs` (`item/delta` carries thread_id,
 * item_id, delta) with realistic UUID-length ids.
 */

const THREAD_ID = "11111111-2222-3333-4444-555555555555";
const ITEM_ID = "66666666-7777-8888-9999-000000000000";

function frame(delta) {
  return JSON.stringify({
    jsonrpc: "2.0",
    method: "item/delta",
    params: { thread_id: THREAD_ID, item_id: ITEM_ID, delta },
  });
}

console.log("--- frame size vs payload, no coalescing (desktop never subscribes) ---");
for (const chunk of [1, 4, 16, 64, 256]) {
  const f = frame("x".repeat(chunk));
  console.log(
    `chunk=${String(chunk).padStart(3)}B  frame=${String(f.length).padStart(4)}B  amplification=${(f.length / chunk).toFixed(1)}x`,
  );
}

console.log(
  "\nThe envelope dominates small deltas: coalescing (currently gated on\n" +
    "`has_subscribed_clients()`, so desktop misses it) is what would cut frame count.",
);