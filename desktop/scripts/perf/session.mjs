/**
 * §1.1(a): the per-delta `findIndex` cost in `Session.apply`.
 *
 * Reproduces the growth curve quoted in the spec. The important caveat (see
 * spec §1.1(a′)) is that a *realistic* interleaved turn does not always address
 * the array tail — `index-lookup.mjs` is the script that proves it.
 */

import { bundle, row } from "./harness.mjs";

const { Session } = await import(bundle("src/lib/session.ts"));

function build(n) {
  const s = new Session();
  for (let i = 0; i < n; i += 1) {
    s.apply({
      method: "item/completed",
      params: {
        thread_id: "t",
        item: { type: "agentMessage", id: `a${i}`, text: "x".repeat(50) },
      },
    });
  }
  return s;
}

function benchDeltaOnTail(s, label) {
  const target = `a${s.items.length - 1}`;
  const N = 2000;
  const t0 = process.hrtime.bigint();
  for (let k = 0; k < N; k += 1) {
    s.apply({
      method: "item/delta",
      params: { thread_id: "t", item_id: target, delta: "y" },
    });
  }
  const t1 = process.hrtime.bigint();
  row(
    `${label}  items=${String(s.items.length).padStart(5)}`,
    `per-delta=${(Number(t1 - t0) / N / 1000).toFixed(2)} us`,
  );
}

console.log("--- item/delta addressed at the array tail ---");
for (const n of [200, 1000, 4000, 8000, 16000]) {
  benchDeltaOnTail(build(n), `n=${String(n).padStart(5)}`);
}