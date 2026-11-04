import { describe, expect, it, vi } from "vitest";

import { BridgeSubscriptionError, EventHub } from "./events";

describe("revisions", () => {
  it("increments monotonically and starts at zero", () => {
    const hub = new EventHub();
    expect(hub.revision("t")).toBe(0);
    expect(hub.publish("t", { n: 1 })).toBe(1);
    expect(hub.publish("t", { n: 2 })).toBe(2);
    expect(hub.revision("t")).toBe(2);
  });

  it("keeps topics independent", () => {
    const hub = new EventHub();
    hub.publish("a", 1);
    hub.publish("a", 2);
    hub.publish("b", 1);
    expect(hub.revision("a")).toBe(2);
    expect(hub.revision("b")).toBe(1);
  });
});

describe("subscribe without a gap", () => {
  it("attaches directly and receives future events", async () => {
    const hub = new EventHub();
    hub.publish("t", { n: 1 });
    const seen: number[] = [];
    const handle = await hub.subscribe<{ n: number }>("t", 1, (event) => seen.push(event.payload.n));

    expect(handle.snapshot).toBeNull();
    expect(handle.revision).toBe(1);

    hub.publish("t", { n: 2 });
    expect(seen).toEqual([2]);
    expect(hub.subscriberCount("t")).toBe(1);
  });

  it("stops delivery after unsubscribe", async () => {
    const hub = new EventHub();
    const seen: number[] = [];
    const handle = await hub.subscribe<number>("t", 0, (event) => seen.push(event.payload));
    hub.publish("t", 1);
    handle.unsubscribe();
    hub.publish("t", 2);
    expect(seen).toEqual([1]);
    expect(hub.subscriberCount("t")).toBe(0);
  });
});

describe("subscribe across a gap", () => {
  it("takes a snapshot and replays only what came after it", async () => {
    const hub = new EventHub({
      snapshot: async (topic) => ({ topic, revision: 5, state: { total: 5 } }),
    });
    for (let i = 0; i < 7; i += 1) hub.publish("t", i + 1);

    const seen: Array<{ revision: number; payload: number }> = [];
    const handle = await hub.subscribe<number>("t", 1, (event) =>
      seen.push({ revision: event.revision, payload: event.payload }),
    );

    expect(handle.snapshot).toEqual({ topic: "t", revision: 5, state: { total: 5 } });
    expect(seen).toEqual([
      { revision: 6, payload: 6 },
      { revision: 7, payload: 7 },
    ]);
  });

  it("refuses to guess when no snapshot source exists", async () => {
    const hub = new EventHub();
    hub.publish("t", 1);
    await expect(hub.subscribe("t", 0, () => {})).rejects.toBeInstanceOf(BridgeSubscriptionError);
  });

  it("rejects a snapshot that belongs to a different topic", async () => {
    const hub = new EventHub({
      snapshot: async () => ({ topic: "other", revision: 3, state: null }),
    });
    hub.publish("t", 1);
    await expect(hub.subscribe("t", 0, () => {})).rejects.toBeInstanceOf(BridgeSubscriptionError);
  });
});

describe("buffer bounds", () => {
  it("keeps at most the configured number of events", () => {
    const hub = new EventHub({ maxBufferedPerTopic: 3 });
    for (let i = 0; i < 10; i += 1) hub.publish("t", i);
    expect(hub.bufferedRevisions("t")).toEqual([8, 9, 10]);
  });
});

describe("cleanup", () => {
  it("counts and removes every subscriber across topics", async () => {
    const hub = new EventHub();
    await hub.subscribe("a", 0, () => {});
    await hub.subscribe("a", 0, () => {});
    await hub.subscribe("b", 0, () => {});
    expect(hub.subscriberCount()).toBe(3);
    expect(hub.clear()).toBe(3);
    expect(hub.subscriberCount()).toBe(0);
  });

  it("does not throw when publishing after a subscriber removed itself", async () => {
    const hub = new EventHub();
    const handler = vi.fn(() => handle.unsubscribe());
    const handle = await hub.subscribe("t", 0, handler);
    hub.publish("t", 1);
    hub.publish("t", 2);
    expect(handler).toHaveBeenCalledTimes(1);
  });
});
