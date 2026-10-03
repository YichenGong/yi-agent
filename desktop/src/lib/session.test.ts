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

  describe("notices", () => {
    it("appends a notice that is not an agent message", () => {
      const s = new Session();
      s.notice("对话已清空");
      expect(s.items).toHaveLength(1);
      const item = s.items[0];
      expect(item.type).toBe("notice");
      expect((item as { text: string }).text).toBe("对话已清空");
    });

    it("gives each notice a distinct id", () => {
      const s = new Session();
      s.notice("a");
      s.notice("b");
      const ids = s.items.map((i) => i.id);
      expect(new Set(ids).size).toBe(2);
    });

    it("drops notices on reset", () => {
      const s = new Session();
      s.notice("x");
      s.reset();
      expect(s.items).toHaveLength(0);
    });
  });

  describe("lastServerItemId", () => {
    it("tracks the id of the newest server item", () => {
      const s = new Session();
      expect(s.lastServerItemId).toBeNull();
      s.apply({
        method: "item/started",
        params: { thread_id: "t", item: { type: "agentMessage", id: "a1", text: "" } },
      });
      expect(s.lastServerItemId).toBe("a1");
      s.apply({
        method: "item/completed",
        params: { thread_id: "t", item: { type: "agentMessage", id: "a1", text: "hi" } },
      });
      expect(s.lastServerItemId).toBe("a1");
      s.apply({
        method: "item/started",
        params: { thread_id: "t", item: { type: "toolCall", id: "i2", call_id: "c2", name: "bash", input: {}, status: "running" } },
      });
      expect(s.lastServerItemId).toBe("i2");
    });

    it("tracks deltas keyed by item_id and clears on reset", () => {
      const s = new Session();
      s.apply({ method: "item/delta", params: { thread_id: "t", item_id: "a9", delta: "x" } });
      expect(s.lastServerItemId).toBe("a9");
      s.reset();
      expect(s.lastServerItemId).toBeNull();
    });

    it("ignores locally minted ids (user message / notice)", () => {
      const s = new Session();
      s.addUserMessage("hi");
      s.notice("out");
      expect(s.lastServerItemId).toBeNull();
    });
  });

  describe("upsertItems", () => {
    it("appends unseen items and replaces seen ones by id", () => {
      const s = new Session();
      s.apply({
        method: "item/started",
        params: { thread_id: "t", item: { type: "agentMessage", id: "a1", text: "" } },
      });
      s.upsertItems([
        { type: "agentMessage", id: "a1", text: "hello" },
        { type: "agentMessage", id: "a2", text: "world" },
      ]);
      expect(s.items).toHaveLength(2);
      expect(s.items[0]).toMatchObject({ id: "a1", text: "hello" });
      expect(s.items[1]).toMatchObject({ id: "a2", text: "world" });
      // 去重：重复 id 不新增。
      s.upsertItems([{ type: "agentMessage", id: "a2", text: "world again" }]);
      expect(s.items).toHaveLength(2);
      expect(s.items[1]).toMatchObject({ id: "a2", text: "world again" });
    });

    it("advances lastServerItemId to the last incoming id", () => {
      const s = new Session();
      s.upsertItems([
        { type: "agentMessage", id: "a1", text: "1" },
        { type: "toolCall", id: "i2", call_id: "c2", name: "bash", input: {}, status: "running" },
      ]);
      expect(s.lastServerItemId).toBe("i2");
    });

    it("is a no-op for an empty list", () => {
      const s = new Session();
      s.apply({
        method: "item/started",
        params: { thread_id: "t", item: { type: "agentMessage", id: "a1", text: "" } },
      });
      s.upsertItems([]);
      expect(s.items).toHaveLength(1);
      expect(s.lastServerItemId).toBe("a1");
    });
  });

  describe("batched items/completed replay", () => {
    it("merges a batched items/completed replay", () => {
      const s = new Session();
      s.apply({
        method: "items/completed",
        params: {
          thread_id: "t1",
          items: [
            { type: "userMessage", id: "user-1", text: "hi" },
            { type: "agentMessage", id: "item-1", text: "hello" },
          ],
        },
      } as never);
      expect(s.items.map((i) => i.id)).toEqual(["user-1", "item-1"]);
    });

    it("batched replay does not duplicate an already-seen item", () => {
      const s = new Session();
      s.apply({
        method: "item/completed",
        params: { thread_id: "t1", item: { type: "agentMessage", id: "item-1", text: "a" } },
      } as never);
      s.apply({
        method: "items/completed",
        params: {
          thread_id: "t1",
          items: [
            { type: "agentMessage", id: "item-1", text: "a" },
            { type: "agentMessage", id: "item-2", text: "b" },
          ],
        },
      } as never);
      expect(s.items.filter((i) => i.id === "item-1")).toHaveLength(1);
      expect(s.items.map((i) => i.id)).toEqual(["item-1", "item-2"]);
    });
  });

  describe("opening user item ordering", () => {
    it("keeps the opening user item ahead of agent items on a cold-open replay", () => {
      const s = new Session();
      // A cold thread the user typed into while it was actually running: only the
      // local echo exists until the full replay lands.
      s.addUserMessage("Implement the plan");
      // The replay is in server order: opener first, then the agent's output.
      s.upsertItems([
        { type: "userMessage", id: "user-t1", text: "Implement the plan" },
        { type: "agentMessage", id: "a1", text: "working" },
      ]);
      expect(s.items.map((i) => i.id)).toEqual(["user-t1", "a1"]);
    });

    it("appends the opener and following items in server order on an empty view", () => {
      const s = new Session();
      s.upsertItems([
        { type: "userMessage", id: "user-t1", text: "Implement the plan" },
        { type: "agentMessage", id: "a1", text: "working" },
      ]);
      expect(s.items.map((i) => i.id)).toEqual(["user-t1", "a1"]);
    });

    it("reconciles the local echo with the replayed opening item instead of duplicating it", () => {
      const s = new Session();
      s.addUserMessage("hello");
      s.upsertItems([{ type: "userMessage", id: "user-t1", text: "hello" }]);
      expect(s.items).toHaveLength(1);
      expect(s.items[0]).toMatchObject({ type: "userMessage", id: "user-t1", text: "hello" });
    });

    it("reconciles the local echo when the opening item arrives live", () => {
      const s = new Session();
      s.addUserMessage("hello");
      s.apply({
        method: "item/started",
        params: { thread_id: "t", item: { type: "userMessage", id: "user-t1", text: "hello" } },
      });
      expect(s.items).toHaveLength(1);
      expect(s.items[0].id).toBe("user-t1");
    });

    it("keeps agent text after the opening user item when both arrive live", () => {
      const s = new Session();
      s.addUserMessage("hello");
      s.apply({
        method: "item/started",
        params: { thread_id: "t", item: { type: "userMessage", id: "user-t1", text: "hello" } },
      });
      s.apply({
        method: "item/started",
        params: { thread_id: "t", item: { type: "agentMessage", id: "a1", text: "" } },
      });
      s.apply({ method: "item/delta", params: { thread_id: "t", item_id: "a1", delta: "hi" } });
      expect(s.items.map((i) => i.id)).toEqual(["user-t1", "a1"]);
    });

    it("drops a duplicate local echo once the server item is present", () => {
      const s = new Session();
      s.addUserMessage("dup");
      s.addUserMessage("dup");
      s.upsertItems([{ type: "userMessage", id: "user-t1", text: "dup" }]);
      expect(s.items).toHaveLength(1);
      expect(s.items[0].id).toBe("user-t1");
    });
  });

  describe("dropLocalUserMessage", () => {
    it("removes the matching local echo", () => {
      const s = new Session();
      s.addUserMessage("hi");
      s.dropLocalUserMessage("hi");
      expect(s.items).toHaveLength(0);
    });

    it("never removes a server item", () => {
      const s = new Session();
      s.upsertItems([{ type: "userMessage", id: "user-t1", text: "hi" }]);
      s.dropLocalUserMessage("hi");
      expect(s.items).toHaveLength(1);
      expect(s.items[0].id).toBe("user-t1");
    });
  });
});
