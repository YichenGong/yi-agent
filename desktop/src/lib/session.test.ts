import { describe, it, expect } from "vitest";
import { Session } from "./session";
import type { Notification } from "./protocol";

describe("Session", () => {
  it("appends a user message locally", () => {
    const s = new Session();
    s.addUserMessage("hi");
    expect(s.items).toHaveLength(1);
    expect(s.items[0]).toMatchObject({ type: "userMessage", text: "hi" });
    // 没有附件时不得凭空多一个 `attachments` 字段：服务端项是权威，本地回声
    // 的字段形状必须与它一致，否则同一条消息会有两种渲染。
    expect((s.items[0] as { attachments?: unknown }).attachments).toBeUndefined();
  });

  it("keeps attachments on the optimistic user bubble", () => {
    const s = new Session();
    s.addUserMessage("总结一下", [
      { name: "报告.pdf", path: ".yi-agent/attachments/t1/a1b2-报告.pdf", size: 10 },
    ]);
    expect(s.items[0]).toMatchObject({
      type: "userMessage",
      text: "总结一下",
      attachments: [{ name: "报告.pdf" }],
    });
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

  it("reports that a tool-output delta changed nothing visible", () => {
    // bash 每 4 KiB 输出就推一帧 `item/delta`（还伴随一次 `force()`）。基线 UI
    // 不渲染工具流式输出，这些帧对转录没有任何可见影响，却曾逐帧触发整棵树
    // 重渲染——长命令（构建日志、`cat` 大文件）会把主线程拖到点不动。
    // `apply` 因此报告「有没有可见变化」，由调用方决定要不要重渲染。
    const s = new Session();
    expect(
      s.apply({
        method: "item/started",
        params: {
          thread_id: "t",
          item: { type: "toolCall", id: "i1", call_id: "c1", name: "bash", input: {}, status: "running" },
        },
      }),
    ).toBe(true);
    expect(
      s.apply({ method: "item/delta", params: { thread_id: "t", item_id: "i1", delta: "stdout" } }),
    ).toBe(false);
    // 助手正文的 delta 是可见的，必须继续报告变化。
    expect(
      s.apply({ method: "item/delta", params: { thread_id: "t", item_id: "a1", delta: "hi" } }),
    ).toBe(true);
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
        total_input_tokens: 0,
        total_output_tokens: 0,
        total_cache_creation_input_tokens: 0,
        total_cache_read_input_tokens: 0,
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

  it("adopts persisted usage only when no live snapshot exists", () => {
    const s = new Session();
    s.adoptPersistedUsage({ model: "m", input: 10, output: 1, cacheRead: 0, cacheWrite: 0 });
    expect(s.usage).toEqual({ model: "m", input: 10, output: 1, cacheRead: 0, cacheWrite: 0 });

    // 持久化值只是一轮的落盘快照；live 通知代表「当前这一轮」，绝不能反过来
    // 被上一轮的旧数据覆盖。
    s.apply({
      method: "thread/tokenUsage/updated",
      params: {
        thread_id: "t",
        model: "m2",
        input_tokens: 99,
        output_tokens: 3,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
        total_input_tokens: 0,
        total_output_tokens: 0,
        total_cache_creation_input_tokens: 0,
        total_cache_read_input_tokens: 0,
      },
    });
    s.adoptPersistedUsage({ model: "old", input: 1, output: 1, cacheRead: 0, cacheWrite: 0 });
    expect(s.usage).toEqual({ model: "m2", input: 99, output: 3, cacheRead: 0, cacheWrite: 0 });
  });

  it("ignores a null persisted usage", () => {
    const s = new Session();
    s.adoptPersistedUsage(null);
    expect(s.usage).toBeNull();
  });

  it("defaults cache tokens to zero when absent", () => {
    // 服务端现在恒发这两个 cache 字段，生成出来的线类型因此把二者标为必需。
    // 本用例的**全部意义**就是「它们缺席时折叠层仍然容错」，所以这里刻意按
    // 改码前的（cache 可选）形状构造载荷，在调用点断言成通知类型。
    const legacyUsage = {
      thread_id: "t",
      model: "m",
      input_tokens: 1,
      output_tokens: 2,
    };
    const s = new Session();
    s.apply({
      method: "thread/tokenUsage/updated",
      params: legacyUsage,
    } as unknown as Notification);
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
      params: { thread_id: "t", model: "m", input_tokens: 100, output_tokens: 0, cache_creation_input_tokens: 0, cache_read_input_tokens: 9, total_input_tokens: 100, total_output_tokens: 2, total_cache_creation_input_tokens: 0, total_cache_read_input_tokens: 9 },
    });
    s.apply({
      method: "thread/tokenUsage/updated",
      params: { thread_id: "t", model: "m", input_tokens: 100, output_tokens: 42, cache_creation_input_tokens: 0, cache_read_input_tokens: 9, total_input_tokens: 100, total_output_tokens: 42, total_cache_creation_input_tokens: 0, total_cache_read_input_tokens: 9 },
    });
    expect(s.usage).toEqual({ model: "m", input: 100, output: 42, cacheRead: 9, cacheWrite: 0 });
  });

  it("keeps cumulative totals from the usage notification", () => {
    const s = new Session();
    s.apply({
      method: "thread/tokenUsage/updated",
      params: {
        thread_id: "t",
        model: "m",
        input_tokens: 10,
        output_tokens: 2,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 5,
        total_input_tokens: 100,
        total_output_tokens: 20,
        total_cache_creation_input_tokens: 1,
        total_cache_read_input_tokens: 50,
      },
    });
    // 快照与累计是两套数：前者算上下文占用，后者是 `/usage` 与状态栏的主体。
    expect(s.usage).toEqual({ model: "m", input: 10, output: 2, cacheRead: 5, cacheWrite: 0 });
    expect(s.usageTotals).toEqual({ input: 100, output: 20, cacheRead: 50, cacheWrite: 1 });
  });

  it("does not overwrite live totals with the persisted baseline", () => {
    const s = new Session();
    s.adoptPersistedTotals({ input: 7, output: 1, cacheRead: 0, cacheWrite: 0 });
    expect(s.usageTotals?.input).toBe(7);
    s.apply({
      method: "thread/tokenUsage/updated",
      params: {
        thread_id: "t",
        model: "m",
        input_tokens: 1,
        output_tokens: 1,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
        total_input_tokens: 8,
        total_output_tokens: 2,
        total_cache_creation_input_tokens: 0,
        total_cache_read_input_tokens: 0,
      },
    });
    // live 通知比落盘值新，绝不能被它覆盖回 7。
    s.adoptPersistedTotals({ input: 7, output: 1, cacheRead: 0, cacheWrite: 0 });
    expect(s.usageTotals).toEqual({ input: 8, output: 2, cacheRead: 0, cacheWrite: 0 });
  });

  it("ignores a null persisted totals", () => {
    const s = new Session();
    s.adoptPersistedTotals(null);
    expect(s.usageTotals).toBeNull();
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

  it("falls back to a labelled cause when the server reports an unknown one", () => {
    // `cause` 在线形状里是 `string`：服务端将来新增一个原因而前端还没跟上时，
    // 收窄必须给出一个**有文案**的成员，否则 `RETRY_CAUSE_LABELS[cause]` 会渲染
    // 成空标签。`network_error` 是这里的兜底。
    const s = new Session();
    s.apply({
      method: "turn/retry",
      params: { thread_id: "t", turn_id: "u1", attempt: 1, max: 3, cause: "brand_new_cause" },
    });
    expect(s.retrying).toEqual({ attempt: 1, max: 3, cause: "network_error" });
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
        item: { type: "userInterjection", id: "interject-u1-1", text: "also do X" },
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
      item: { type: "userInterjection", id: "interject-u1-1", text: "also do X" },
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
      params: { thread_id: "t", model: "m", input_tokens: 1, output_tokens: 2, cache_creation_input_tokens: 0, cache_read_input_tokens: 0, total_input_tokens: 1, total_output_tokens: 2, total_cache_creation_input_tokens: 0, total_cache_read_input_tokens: 0 },
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

    it("carries a structured config payload on a notice", () => {
      const s = new Session();
      s.noticeBody({ kind: "config", data: { model: "m" }, error: null });
      const item = s.items[0] as {
        type: string;
        text: string;
        body?: { kind: string; data?: unknown; error?: unknown };
      };
      expect(item.type).toBe("notice");
      expect(item.text).toBe("");
      expect(item.body?.kind).toBe("config");
      expect(item.body?.data).toEqual({ model: "m" });
      expect(item.body?.error).toBeNull();
    });

    it("carries a config read error", () => {
      const s = new Session();
      s.noticeBody({ kind: "config", data: null, error: "boom" });
      const item = s.items[0] as {
        body?: { kind: string; data?: unknown; error?: unknown };
      };
      expect(item.body?.kind).toBe("config");
      expect(item.body?.data).toBeNull();
      expect(item.body?.error).toBe("boom");
    });

    it("carries a help payload with no target", () => {
      const s = new Session();
      s.noticeBody({ kind: "help", target: null, error: null });
      const item = s.items[0] as {
        text: string;
        body?: { kind: string; target?: unknown; error?: unknown };
      };
      // 卡片就是呈现：`text` 保持空串，没有别的消费者读它。
      expect(item.text).toBe("");
      expect(item.body?.kind).toBe("help");
      expect(item.body?.target).toBeNull();
      expect(item.body?.error).toBeNull();
    });

    it("carries a help error line", () => {
      const s = new Session();
      s.noticeBody({ kind: "help", target: null, error: "未知命令: /nope" });
      const item = s.items[0] as { body?: { error?: unknown } };
      expect(item.body?.error).toBe("未知命令: /nope");
    });
  });

  describe("reasoning stream", () => {
    it("merges consecutive reasoning deltas into one block, concatenating the text", () => {
      const s = new Session();
      s.apply({
        method: "thread/reasoningDelta",
        params: { thread_id: "t", text: "first ", kind: "Content" },
      });
      s.apply({
        method: "thread/reasoningDelta",
        params: { thread_id: "t", text: "second", kind: "Content" },
      });
      // CoT 是一块，不是一 token 一块——与 TUI 的单 `Reasoning` cell 同契约。
      const blocks = s.items.filter((i) => i.type === "reasoning");
      expect(blocks).toHaveLength(1);
      expect((blocks[0] as { text: string }).text).toBe("first second");
    });

    it("starts a new block when non-reasoning content lands in between", () => {
      const s = new Session();
      s.apply({
        method: "thread/reasoningDelta",
        params: { thread_id: "t", text: "think", kind: "Content" },
      });
      s.apply({
        method: "item/started",
        params: { thread_id: "t", item: { type: "agentMessage", id: "a1", text: "" } },
      });
      s.apply({
        method: "thread/reasoningDelta",
        params: { thread_id: "t", text: "more", kind: "Summary" },
      });
      expect(s.items.filter((i) => i.type === "reasoning")).toHaveLength(2);
    });

    it("never lets reasoning text leak into the agent message", () => {
      const s = new Session();
      s.apply({
        method: "thread/reasoningDelta",
        params: { thread_id: "t", text: "private", kind: "Content" },
      });
      s.apply({ method: "item/delta", params: { thread_id: "t", item_id: "a1", delta: "answer" } });
      const agent = s.items.find((i) => i.type === "agentMessage") as { text: string };
      expect(agent.text).toBe("answer");
    });

    it("keeps the block before the answer when the send path opens with a user item", () => {
      // 真实发送路径：本地回声 → 服务端开启项(user) → CoT 增量 → 开启项(agent) → 正文
      // 增量。上面那条「无先行服务端项」的用例里块恰好落在末尾，看不出锚点问题；
      // 这里块必须停在用户气泡之后、**正文之前**（design §6.1「位于正文之前」），
      // 而不是被随后到达的 agent 项甩到转录末尾。
      const s = new Session();
      s.addUserMessage("q1");
      s.apply({
        method: "item/started",
        params: { thread_id: "t", item: { type: "userMessage", id: "user-t1", text: "q1" } },
      });
      s.apply({
        method: "thread/reasoningDelta",
        params: { thread_id: "t", text: "T1a", kind: "Content" },
      });
      s.apply({
        method: "thread/reasoningDelta",
        params: { thread_id: "t", text: "T1b", kind: "Content" },
      });
      s.apply({
        method: "item/started",
        params: { thread_id: "t", item: { type: "agentMessage", id: "a1", text: "" } },
      });
      s.apply({ method: "item/delta", params: { thread_id: "t", item_id: "a1", delta: "A1" } });

      expect(s.items.map((i) => i.type)).toEqual(["userMessage", "reasoning", "agentMessage"]);
      expect(s.items.map((i) => i.id)).toEqual(["user-t1", expect.stringMatching(/^local-/), "a1"]);
      // 连续 delta 仍是一块。
      expect((s.items[1] as { text: string }).text).toBe("T1aT1b");
      expect((s.items[2] as { text: string }).text).toBe("A1");
    });

    it("starts a separate block per turn instead of stranding one merged block at the tail", () => {
      // 两轮：每轮的 CoT 各自成块，且都落在自己那一轮的答案之前
      // （reasoning[T1] | agentMessage[A1] | reasoning[T2] | agentMessage[A2]）。
      const s = new Session();
      s.addUserMessage("q1");
      s.apply({
        method: "item/started",
        params: { thread_id: "t", item: { type: "userMessage", id: "user-t1", text: "q1" } },
      });
      for (const text of ["T1a", "T1b"]) {
        s.apply({ method: "thread/reasoningDelta", params: { thread_id: "t", text, kind: "Content" } });
      }
      s.apply({
        method: "item/started",
        params: { thread_id: "t", item: { type: "agentMessage", id: "a1", text: "" } },
      });
      s.apply({ method: "item/delta", params: { thread_id: "t", item_id: "a1", delta: "A1" } });
      s.apply({
        method: "item/completed",
        params: { thread_id: "t", item: { type: "agentMessage", id: "a1", text: "A1" } },
      });

      s.addUserMessage("q2");
      s.apply({
        method: "item/started",
        params: { thread_id: "t", item: { type: "userMessage", id: "user-t2", text: "q2" } },
      });
      s.apply({
        method: "thread/reasoningDelta",
        params: { thread_id: "t", text: "T2a", kind: "Content" },
      });
      s.apply({
        method: "item/started",
        params: { thread_id: "t", item: { type: "agentMessage", id: "a2", text: "" } },
      });
      s.apply({ method: "item/delta", params: { thread_id: "t", item_id: "a2", delta: "A2" } });

      expect(s.items.map((i) => i.type)).toEqual([
        "userMessage",
        "reasoning",
        "agentMessage",
        "userMessage",
        "reasoning",
        "agentMessage",
      ]);
      expect(s.items.map((i) => (i.type === "reasoning" ? (i as { text: string }).text : i.id))).toEqual([
        "user-t1",
        "T1aT1b",
        "a1",
        "user-t2",
        "T2a",
        "a2",
      ]);
    });

    it("hands the open local block over to the server's thinking item in place", () => {
      // 服务端 thinking item 是权威：实时期间本地推理块只保留流式手感，item 一到
      // 就**就地接管**（位置不变、记服务端 id），其后 delta 不再并入它。
      const s = new Session();
      s.addUserMessage("q1");
      s.apply({
        method: "item/started",
        params: { thread_id: "t", item: { type: "userMessage", id: "user-t1", text: "q1" } },
      });
      for (const text of ["T1a", "T1b"]) {
        s.apply({ method: "thread/reasoningDelta", params: { thread_id: "t", text, kind: "Content" } });
      }
      expect(s.items.map((i) => i.type)).toEqual(["userMessage", "reasoning"]);

      s.apply({
        method: "item/started",
        params: {
          thread_id: "t",
          item: { type: "thinking", id: "item-turn1-2", text: "T1aT1b", redacted: false },
        },
      });
      // 就地在原推理块的位置换成服务端项——不是新起一块、也不是丢到末尾。
      expect(s.items.map((i) => i.type)).toEqual(["userMessage", "thinking"]);
      expect(s.items.map((i) => i.id)).toEqual(["user-t1", "item-turn1-2"]);
      expect(s.items[1]).toMatchObject({ type: "thinking", text: "T1aT1b", redacted: false });

      // 之后的 delta 不再并入已接管的块：另起一块本地推理块。
      s.apply({ method: "thread/reasoningDelta", params: { thread_id: "t", text: "T2", kind: "Content" } });
      expect(s.items.map((i) => i.type)).toEqual(["userMessage", "thinking", "reasoning"]);
    });

    it("does not take over a fresh local block on the paired item/completed", () => {
      // 防御：started 已接管并断开运行；若其后（completed 之前）又来了 delta，
      // 会新起一个本地块 B2。completed 绝不能把 B2 也误当作「当前打开的块」接管
      // ——否则同一服务端 id 会重复，且 B2 的内容被吞。
      const s = new Session();
      s.apply({ method: "thread/reasoningDelta", params: { thread_id: "t", text: "T1", kind: "Content" } });
      const started = { type: "thinking", id: "item-turn1-1", text: "T1", redacted: false } as const;
      s.apply({ method: "item/started", params: { thread_id: "t", item: started } });
      // completed 之前到达的新 delta：另起本地块。
      s.apply({ method: "thread/reasoningDelta", params: { thread_id: "t", text: "T2", kind: "Content" } });
      expect(s.items.map((i) => i.type)).toEqual(["thinking", "reasoning"]);
      // 配对的 completed 只能按 id 就地替换已接管的服务端项，不碰 B2。
      s.apply({ method: "item/completed", params: { thread_id: "t", item: started } });
      expect(s.items.map((i) => i.type)).toEqual(["thinking", "reasoning"]);
      expect(s.items.filter((i) => i.type === "thinking")).toHaveLength(1);
      expect((s.items[1] as { text: string }).text).toBe("T2");
    });

    it("replaces the taken-over item on the paired item/completed without duplicating", () => {
      const s = new Session();
      s.apply({ method: "thread/reasoningDelta", params: { thread_id: "t", text: "cot", kind: "Content" } });
      const item = { type: "thinking", id: "item-turn1-1", text: "cot", redacted: false } as const;
      s.apply({ method: "item/started", params: { thread_id: "t", item } });
      s.apply({ method: "item/completed", params: { thread_id: "t", item } });
      const thinking = s.items.filter((i) => i.type === "thinking");
      expect(thinking).toHaveLength(1);
      expect(thinking[0].id).toBe("item-turn1-1");
    });

    it("replays a thinking item on a cold resume", () => {
      // 回放期没有打开的本地块：thinking item 走常规服务端项路径落到转录里，
      // 使历史会话的思考卡片可被渲染（design §6.1）。
      const s = new Session();
      s.upsertItems([
        { type: "userMessage", id: "user-t1", text: "q1" },
        { type: "thinking", id: "item-turn1-2", text: "历史思考", redacted: false },
        { type: "agentMessage", id: "a1", text: "A1" },
      ]);
      expect(s.items.map((i) => i.type)).toEqual(["userMessage", "thinking", "agentMessage"]);
      expect(s.items[1]).toMatchObject({ type: "thinking", text: "历史思考" });
      expect(s.lastServerItemId).toBe("a1");
    });
  });

  describe("server notices", () => {
    it("pushes a notice carrying its level", () => {
      const s = new Session();
      s.apply({
        method: "thread/notice",
        params: { thread_id: "t", level: "Warn", text: "该模型不支持所配推理档，已忽略" },
      });
      expect(s.items).toHaveLength(1);
      expect(s.items[0]).toMatchObject({
        type: "notice",
        text: "该模型不支持所配推理档，已忽略",
        level: "Warn",
      });
    });
  });

  describe("effort state", () => {
    it("does not keep a session-level override (the live state is ThreadView.reasoningEffort)", () => {
      // `thread/reasoningEffortChanged` 由 `ThreadStore.applyNotification` 在到达
      // `Session.apply` 之前截走（见 threadStore.ts），会话元数据住在
      // `ThreadView.reasoningEffort`。这里曾经有一个同名 Session 字段，但它在本
      // 应用的调用路径上永远到不了，读起来像「活的覆盖」其实是死的——删掉以免
      // 误以为有覆盖。此用例钉住「Session 不再是该状态的持有者」。
      const s = new Session();
      s.apply({
        method: "thread/reasoningEffortChanged",
        params: { thread_id: "t", effort: "high" },
      });
      expect((s as unknown as { reasoningEffort?: unknown }).reasoningEffort).toBeUndefined();
    });

    it("ignores the effort notification without disturbing the transcript", () => {
      const s = new Session();
      s.notice("before");
      s.apply({ method: "thread/reasoningEffortChanged", params: { thread_id: "t" } });
      expect(s.items).toHaveLength(1);
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

  describe("Session replay reconciliation", () => {
    const serverItem = (id: string, text: string) => ({
      method: "item/completed",
      params: { item: { type: "agentMessage", id, text } },
    });

    it("collects server ids during a replay and drops non-replayed ones", () => {
      const s = new Session();
      // Prime the session with a stale cache: A, B, C plus a local echo + a notice.
      s.upsertItems([
        { type: "agentMessage", id: "A", text: "a" },
        { type: "agentMessage", id: "B", text: "b" },
        { type: "agentMessage", id: "C", text: "c" },
      ]);
      s.notice("cmd");
      s.addUserMessage("hello");
      const notices = s.items.filter((i) => i.type === "notice");
      const localEchoes = s.items.filter((i) => i.id.startsWith("local-") && i.type === "userMessage");

      s.beginReplay();
      s.apply(serverItem("A", "a") as never);
      s.apply(serverItem("C", "c") as never);
      const ids = s.endReplay();
      expect(ids).toEqual(new Set(["A", "C"]));

      s.dropStale(ids!);
      expect(s.items.filter((i) => i.type !== "notice" && !i.id.startsWith("local-")).map((i) => i.id))
        .toEqual(["A", "C"]);
      // Local echoes and notices survive reconciliation.
      expect(s.items.filter((i) => i.type === "notice")).toHaveLength(notices.length);
      expect(s.items.filter((i) => i.id.startsWith("local-") && i.type === "userMessage"))
        .toHaveLength(localEchoes.length);
    });

    it("endReplay is null when no replay was started, and dropStale drops a server item absent from the replay", () => {
      const s = new Session();
      s.upsertItems([{ type: "agentMessage", id: "A", text: "a" }]);
      expect(s.endReplay()).toBeNull();
      // An empty replay set cannot be a no-op: `dropStale` is only ever reached
      // with the completed set after a *successful* full replay, and a cached
      // server item the server did not re-send is a ghost. The sibling test
      // "clears lastServerItemId when nothing server-authored survives" pins the
      // same rule for the cursor, so the two cannot disagree here.
      s.dropStale(new Set());
      expect(s.items.map((i) => i.id)).toEqual([]);
    });

    it("does not collect ids outside a replay", () => {
      const s = new Session();
      s.apply(serverItem("X", "x") as never);
      expect(s.endReplay()).toBeNull();
    });

    it("recomputes lastServerItemId from the surviving items", () => {
      const s = new Session();
      s.upsertItems([
        { type: "agentMessage", id: "A", text: "a" },
        { type: "agentMessage", id: "B", text: "b" },
      ]);
      s.dropStale(new Set(["A"]));
      expect(s.lastServerItemId).toBe("A");
    });

    it("clears lastServerItemId when nothing server-authored survives", () => {
      const s = new Session();
      s.upsertItems([{ type: "agentMessage", id: "B", text: "b" }]);
      s.dropStale(new Set());
      expect(s.lastServerItemId).toBeNull();
    });
  });
});

describe("Session.adoptEpoch", () => {
  it("drops the whole transcript when the epoch changed", () => {
    const s = new Session();
    s.adoptEpoch(1, "item-1");
    s.items.push({ type: "agentMessage", id: "item-1", text: "old" });
    s.lastServerItemId = "item-1";

    const invalidated = s.adoptEpoch(2, null);

    expect(invalidated).toBe(true);
    expect(s.items).toEqual([]);
    expect(s.lastServerItemId).toBeNull();
    expect(s.contentEpoch).toBe(2);
  });

  it("only advances the cursor when the epoch matches", () => {
    const s = new Session();
    s.adoptEpoch(1, "item-1");
    s.items.push({ type: "agentMessage", id: "item-1", text: "keep" });

    const invalidated = s.adoptEpoch(1, "item-2");

    expect(invalidated).toBe(false);
    expect(s.items).toHaveLength(1);
    expect(s.lastServerItemId).toBe("item-2");
  });

  it("records the epoch without invalidating on the first sighting", () => {
    const s = new Session();
    s.items.push({ type: "agentMessage", id: "item-1", text: "live" });

    const invalidated = s.adoptEpoch(3, "item-1");

    expect(invalidated).toBe(false);
    expect(s.items).toHaveLength(1);
    expect(s.contentEpoch).toBe(3);
  });

  it("sets epochInvalidated so a late cache read cannot re-inject stale items", () => {
    const s = new Session();
    s.adoptEpoch(1, "item-1");
    s.adoptEpoch(2, null);
    expect(s.epochInvalidated).toBe(true);
  });

  it("clears epochInvalidated on reset", () => {
    const s = new Session();
    s.adoptEpoch(1, "item-1");
    s.adoptEpoch(2, null);
    s.reset();
    expect(s.epochInvalidated).toBe(false);
  });
});

describe("Session structuralVersion", () => {
  it("does not change when streamed text is appended in place", () => {
    const s = new Session();
    s.apply({
      method: "item/started",
      params: { thread_id: "t", item: { type: "agentMessage", id: "a1", text: "" } },
    });
    const before = s.structuralVersion;
    s.apply({ method: "item/delta", params: { thread_id: "t", item_id: "a1", delta: "x" } });
    // A pure append mutates `item.text` in place: the history prefix above it
    // has not moved, so the frozen subtree must stay frozen.
    expect(s.structuralVersion).toBe(before);
  });

  it("changes when an item is replaced in place on item/completed", () => {
    const s = new Session();
    s.apply({
      method: "item/started",
      params: { thread_id: "t", item: { type: "agentMessage", id: "a1", text: "hi" } },
    });
    const before = s.structuralVersion;
    s.apply({
      method: "item/completed",
      params: { thread_id: "t", item: { type: "agentMessage", id: "a1", text: "hi there" } },
    });
    expect(s.structuralVersion).toBeGreaterThan(before);
  });

  it("changes when a reasoning block is taken over by a server item", () => {
    const s = new Session();
    s.apply({ method: "thread/reasoningDelta", params: { thread_id: "t", text: "think", kind: "Content" } });
    const before = s.structuralVersion;
    s.apply({
      method: "item/started",
      params: { thread_id: "t", item: { type: "thinking", id: "th1", text: "think", redacted: false } },
    });
    expect(s.structuralVersion).toBeGreaterThan(before);
  });

  it("changes when an in-place delta targets an item that is no longer the tail", () => {
    const s = new Session();
    s.apply({
      method: "item/started",
      params: { thread_id: "t", item: { type: "agentMessage", id: "a1", text: "" } },
    });
    s.apply({ method: "item/delta", params: { thread_id: "t", item_id: "a1", delta: "first " } });
    // The delta above targeted the tail, so the frozen prefix stayed valid.
    expect(s.structuralVersion).toBe(1);
    // CoT resumes: the block lands at the tail and the still-streaming message
    // is displaced to `length - 2`. `ChatView` only lifts the tail item's text
    // out as a prop, so the displaced one needs the prefix rebuilt.
    s.apply({
      method: "thread/reasoningDelta",
      params: { thread_id: "t", text: "hmm", kind: "Content" },
    });
    const afterBlock = s.structuralVersion;
    s.apply({ method: "item/delta", params: { thread_id: "t", item_id: "a1", delta: "second " } });
    expect(s.structuralVersion).toBeGreaterThan(afterBlock);
  });

  it("changes when a locally minted bubble is dropped", () => {
    const s = new Session();
    s.addUserMessage("oops");
    const before = s.structuralVersion;
    s.dropLocalUserMessage("oops");
    expect(s.structuralVersion).toBeGreaterThan(before);
  });
});
