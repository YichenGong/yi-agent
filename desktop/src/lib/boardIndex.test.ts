import { describe, expect, it } from "vitest";
import { boardErrorKind, kanbanItemFor, summarize } from "./boardIndex";

describe("boardIndex", () => {
  it("只给已登记的项目显示看板条目", () => {
    expect(kanbanItemFor("/a", ["/a", "/b"])).toBe(true);
    expect(kanbanItemFor("/c", ["/a", "/b"])).toBe(false);
  });

  it("摘要把排队与运行分开数", () => {
    expect(summarize([{ state: "queued" }, { state: "queued" }, { state: "running" }]))
      .toBe("2 排队 · 1 运行中");
    expect(summarize([])).toBe("空");
  });
});

describe("boardIndex membership", () => {
  it("是精确匹配，不是前缀匹配", () => {
    // 登记的是 /a 时，/a/b 是一个没有看板的项目——按前缀匹配会让子目录
    // 冒出看板条目，点开后是别人的队列。
    expect(kanbanItemFor("/a/b", ["/a"])).toBe(false);
    expect(kanbanItemFor("/a/", ["/a"])).toBe(false);
  });

  it("空登记表不给任何项目看板条目", () => {
    expect(kanbanItemFor("/a", [])).toBe(false);
  });
});

describe("summarize", () => {
  it("数出还在队列里的每一类，包括合并卡", () => {
    const cards = [
      { state: "queued" },
      { state: "queued" },
      { state: "running" },
      { state: "merging" },
      { state: "awaiting_merge" },
      { state: "needs_you" },
    ];
    expect(summarize(cards)).toBe("2 排队 · 2 运行中 · 1 待合并 · 1 待处理");
  });

  it("合并卡不再被算成 0", () => {
    // 用户报的原始 case：3 张等待合并 + 2 张待处理，旧口径显示「0 排队 · 0 运行中」，
    // 于是卡住的两张合并卡在侧栏完全隐形。
    const cards = [
      { state: "awaiting_merge" },
      { state: "awaiting_merge" },
      { state: "awaiting_merge" },
      { state: "needs_you" },
      { state: "needs_you" },
    ];
    expect(summarize(cards)).toBe("3 待合并 · 2 待处理");
  });

  it("没有待办时明说，不留空", () => {
    expect(summarize([{ state: "done" }, { state: "failed" }])).toBe("无待办");
  });

  it("大小写不敏感（服务端可能回大写）", () => {
    expect(summarize([{ state: "QUEUED" }, { state: "Running" }])).toBe("1 排队 · 1 运行中");
  });
});

describe("boardErrorKind", () => {
  it("区分「没建看板」与「插件没装」", () => {
    expect(boardErrorKind(new Error("board_not_created"))).toBe("not_created");
    expect(boardErrorKind(new Error("daemon_unavailable"))).toBe("daemon_down");
    expect(boardErrorKind(new Error("plugin superpowers-kanban is not available"))).toBe("plugin_missing");
    expect(boardErrorKind(new Error("boom"))).toBe("other");
  });

  it("读得到结构化错误码：desktop 的 RpcClient 抛的是整个 RpcError 对象", () => {
    // RpcError = { code: number, message: string, data?: unknown }，看板语义的
    // 码放在 data.code 里。message 是人话，顺带带上码，两条路都要能认。
    expect(
      boardErrorKind({ code: -32603, message: "看板不存在", data: { code: "board_not_created" } }),
    ).toBe("not_created");
    expect(boardErrorKind({ code: -32603, message: "连不上", data: { code: "daemon_unavailable" } }))
      .toBe("daemon_down");
    expect(boardErrorKind({ code: -32603, message: "没有这个插件", data: { code: "plugin_unavailable" } }))
      .toBe("plugin_missing");
  });

  it("认得出「插件没装」的两种既有措辞", () => {
    expect(boardErrorKind(new Error("PluginUnavailable { plugin: \"x\" }"))).toBe("plugin_missing");
    expect(
      boardErrorKind(
        new Error("the plugin rejected the query: NotFound plugin superpowers-kanban is not available"),
      ),
    ).toBe("plugin_missing");
  });

  it("认得出宿主现在的 daemon 不可达措辞", () => {
    // Task 4 之前 app-server 只给这句话（没有 data.code），UI 不能因此把它
    // 说成「插件没装」。
    expect(boardErrorKind(new Error("daemon is unavailable: connection refused"))).toBe(
      "daemon_down",
    );
  });

  it("非 Error 的输入落回 other，而不是抛异常", () => {
    expect(boardErrorKind(undefined)).toBe("other");
    expect(boardErrorKind(null)).toBe("other");
    expect(boardErrorKind("随便一句")).toBe("other");
  });
});
