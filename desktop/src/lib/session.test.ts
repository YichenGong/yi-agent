import { describe, it, expect } from "vitest";
import { Session } from "./session";

describe("Session", () => {
  it("appends a user message locally", () => {
    const s = new Session();
    s.addUserMessage("hi");
    expect(s.items).toHaveLength(1);
    expect(s.items[0]).toMatchObject({ type: "userMessage", text: "hi" });
  });

  it("appends streamed agent text into one item", () => {
    const s = new Session();
    s.apply({
      method: "item/started",
      params: { thread_id: "t", item: { type: "agentMessage", id: "a1", text: "" } },
    });
    s.apply({ method: "item/delta", params: { thread_id: "t", item_id: "a1", delta: "Hel" } });
    s.apply({ method: "item/delta", params: { thread_id: "t", item_id: "a1", delta: "lo" } });
    expect(s.items).toHaveLength(1);
    expect(s.items[0]).toMatchObject({ type: "agentMessage", text: "Hello" });
  });

  it("updates a tool call item in place on item/completed", () => {
    const s = new Session();
    s.apply({
      method: "item/started",
      params: {
        thread_id: "t",
        item: { type: "toolCall", id: "i1", call_id: "c1", name: "bash", input: {}, status: "running" },
      },
    });
    s.apply({
      method: "item/completed",
      params: {
        thread_id: "t",
        item: {
          type: "toolCall",
          id: "i1",
          call_id: "c1",
          name: "bash",
          input: {},
          status: "completed",
          result: "ok",
        },
      },
    });
    expect(s.items).toHaveLength(1);
    expect(s.items[0]).toMatchObject({ type: "toolCall", status: "completed", result: "ok" });
  });

  it("ignores deltas targeting a non-agent item", () => {
    const s = new Session();
    s.apply({
      method: "item/started",
      params: {
        thread_id: "t",
        item: { type: "toolCall", id: "i1", call_id: "c1", name: "bash", input: {}, status: "running" },
      },
    });
    s.apply({ method: "item/delta", params: { thread_id: "t", item_id: "i1", delta: "stdout" } });
    expect(s.items).toHaveLength(1);
    expect(s.items[0]).toMatchObject({ type: "toolCall", status: "running" });
  });

  it("creates an agent message when a delta arrives with no prior item", () => {
    const s = new Session();
    s.apply({ method: "item/delta", params: { thread_id: "t", item_id: "a9", delta: "hi" } });
    expect(s.items).toHaveLength(1);
    expect(s.items[0]).toMatchObject({ type: "agentMessage", id: "a9", text: "hi" });
  });

  it("tracks turn lifecycle", () => {
    const s = new Session();
    expect(s.turnActive).toBe(false);
    s.apply({ method: "turn/started", params: { thread_id: "t", turn_id: "u1" } });
    expect(s.turnActive).toBe(true);
    s.apply({
      method: "turn/completed",
      params: { thread_id: "t", turn_id: "u1", status: "completed" },
    });
    expect(s.turnActive).toBe(false);
    expect(s.lastStatus).toBe("completed");
  });

  it("records token usage", () => {
    const s = new Session();
    s.apply({
      method: "thread/tokenUsage/updated",
      params: {
        thread_id: "t",
        model: "m",
        input_tokens: 10,
        output_tokens: 3,
        cache_creation_input_tokens: 100,
        cache_read_input_tokens: 200,
      },
    });
    expect(s.usage).toEqual({
      model: "m",
      input: 10,
      output: 3,
      cacheWrite: 100,
      cacheRead: 200,
    });
  });

  it("defaults cache tokens to zero when absent", () => {
    const s = new Session();
    s.apply({
      method: "thread/tokenUsage/updated",
      params: { thread_id: "t", model: "m", input_tokens: 1, output_tokens: 2 },
    });
    expect(s.usage).toEqual({
      model: "m",
      input: 1,
      output: 2,
      cacheRead: 0,
      cacheWrite: 0,
    });
  });

  it("keeps the latest cumulative usage snapshot", () => {
    const s = new Session();
    s.apply({
      method: "thread/tokenUsage/updated",
      params: { thread_id: "t", model: "m", input_tokens: 100, output_tokens: 0, cache_read_input_tokens: 9 },
    });
    s.apply({
      method: "thread/tokenUsage/updated",
      params: { thread_id: "t", model: "m", input_tokens: 100, output_tokens: 42, cache_read_input_tokens: 9 },
    });
    expect(s.usage).toEqual({ model: "m", input: 100, output: 42, cacheRead: 9, cacheWrite: 0 });
  });

  it("records an error notification", () => {
    const s = new Session();
    s.apply({ method: "error", params: { message: "boom" } });
    expect(s.lastError).toBe("boom");
  });

  it("records the error from a failed turn", () => {
    const s = new Session();
    s.apply({ method: "turn/started", params: { thread_id: "t", turn_id: "u1" } });
    s.apply({
      method: "turn/completed",
      params: { thread_id: "t", turn_id: "u1", status: "failed", error: "kaboom" },
    });
    expect(s.turnActive).toBe(false);
    expect(s.lastStatus).toBe("failed");
    expect(s.lastError).toBe("kaboom");
  });

  it("clears lastError when a new turn starts", () => {
    const s = new Session();
    s.apply({ method: "error", params: { message: "boom" } });
    s.apply({ method: "turn/started", params: { thread_id: "t", turn_id: "u1" } });
    expect(s.lastError).toBeNull();
  });

  it("records a retry notice and clears it when text resumes", () => {
    const s = new Session();
    s.apply({
      method: "turn/retry",
      params: {
        thread_id: "t",
        turn_id: "u1",
        attempt: 1,
        max: 3,
        cause: "request_timeout",
      },
    });
    // The cause is carried through so the notice can name the failure mode.
    expect(s.retrying).toEqual({ attempt: 1, max: 3, cause: "request_timeout" });

    s.apply({ method: "item/delta", params: { thread_id: "t", item_id: "a1", delta: "hi" } });
    expect(s.retrying).toBeNull();
  });

  it("clears a retry notice when the turn completes", () => {
    const s = new Session();
    s.apply({
      method: "turn/retry",
      params: { thread_id: "t", turn_id: "u1", attempt: 2, max: 3, cause: "idle_stall" },
    });
    s.apply({
      method: "turn/completed",
      params: { thread_id: "t", turn_id: "u1", status: "completed" },
    });
    expect(s.retrying).toBeNull();
  });

  it("renders a mid-turn interjection as its own user bubble", () => {
    const s = new Session();
    s.apply({ method: "turn/started", params: { thread_id: "t", turn_id: "u1" } });
    s.apply({
      method: "item/started",
      params: {
        thread_id: "t",
        turn_id: "u1",
        item: { type: "user_interjection", id: "interject-u1-1", text: "also do X" },
      },
    } as never);

    const texts = s.items
      .filter((i) => i.type === "userMessage")
      .map((i) => (i as { text: string }).text);
    expect(texts).toContain("also do X");
    expect(s.items).toHaveLength(1);
  });

  it("keeps item/completed from duplicating an interjection bubble", () => {
    // Both `item/started` and `item/completed` carry the same id, so the second
    // must replace rather than append (the shared item path handles this).
    const s = new Session();
    const params = {
      thread_id: "t",
      item: { type: "user_interjection", id: "interject-u1-1", text: "also do X" },
    };
    s.apply({ method: "item/started", params } as never);
    s.apply({ method: "item/completed", params } as never);
    expect(s.items).toHaveLength(1);
  });

  it("restores returned interjections to the pending input", () => {
    const s = new Session();
    s.apply({ method: "turn/started", params: { thread_id: "t", turn_id: "u1" } });
    s.apply({
      method: "turn/interjectionsReturned",
      params: { thread_id: "t", turn_id: "u1", items: ["interject-u1-1"] },
    } as never);

    expect(s.returnedInterjections).toEqual(["interject-u1-1"]);
  });

  it("records an empty return list as nothing pending", () => {
    const s = new Session();
    s.returnedInterjections = ["stale"];
    s.apply({
      method: "turn/interjectionsReturned",
      params: { thread_id: "t", turn_id: "u1", items: [] },
    } as never);
    expect(s.returnedInterjections).toEqual([]);
  });

  it("reset clears all state and starts a fresh items array", () => {
    const s = new Session();
    const before = s.items;
    s.addUserMessage("hi");
    s.apply({ method: "turn/started", params: { thread_id: "t", turn_id: "u1" } });
    s.apply({
      method: "thread/tokenUsage/updated",
      params: { thread_id: "t", model: "m", input_tokens: 1, output_tokens: 2 },
    });
    s.apply({ method: "error", params: { message: "boom" } });
    s.apply({
      method: "turn/completed",
      params: { thread_id: "t", turn_id: "u1", status: "completed" },
    });
    expect(s.lastStatus).not.toBeNull();

    s.reset();

    expect(s.items).not.toBe(before);
    expect(s.items).toHaveLength(0);
    expect(s.turnActive).toBe(false);
    expect(s.lastStatus).toBeNull();
    expect(s.lastError).toBeNull();
    expect(s.usage).toBeNull();
    expect(s.returnedInterjections).toEqual([]);
  });
});
