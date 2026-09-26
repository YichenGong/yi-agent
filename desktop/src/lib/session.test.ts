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
      params: { thread_id: "t", model: "m", input_tokens: 10, output_tokens: 3 },
    });
    expect(s.usage).toEqual({ model: "m", input: 10, output: 3 });
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
  });
});
