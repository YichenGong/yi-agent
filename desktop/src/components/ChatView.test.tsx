/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, cleanup, screen, fireEvent, waitFor } from "@testing-library/react";
import type { Item } from "../lib/protocol";
import type { NoticeItem } from "../lib/session";
import { Session } from "../lib/session";

// Count how often the settled tool card's component body runs. A streamed delta
// on the trailing agent message must not touch items that cannot have changed.
const calls = vi.hoisted(() => ({ tool: 0, text: 0 }));

vi.mock("./ToolCallCard", () => ({
  ToolCallCard: ({ item }: { item: { name: string } }) => {
    calls.tool += 1;
    return <div data-testid="tool">{item.name}</div>;
  },
}));

vi.mock("./MarkdownText", () => ({
  AgentMessage: ({ text }: { text: string }) => {
    // Counted so a streamed delta on the tail cannot hide a re-render of the
    // settled messages above it.
    calls.text += 1;
    return <div>{text}</div>;
  },
}));

// 图片字节由 `image/read` 另取（见 useImageData），此处只验证气泡把 hook 给出的
// URL 画成一个 <img>。真实的分片/缓存行为在 useImageData.test.ts 里测。
vi.mock("../lib/useImageData", () => ({
  useImageData: () => ({ url: "blob:test-image", error: null }),
}));

import { ChatView } from "./ChatView";

afterEach(() => {
  cleanup();
  calls.tool = 0;
  calls.text = 0;
});

function items(): Item[] {
  return [
    { type: "userMessage", id: "u1", text: "do the thing" },
    {
      type: "toolCall",
      id: "tc1",
      call_id: "c1",
      name: "bash",
      input: { cmd: "ls" },
      status: "completed",
      result: "ok",
    },
    { type: "agentMessage", id: "a1", text: "" },
  ];
}

const setText = (list: Item[], text: string) => {
  (list[2] as { type: "agentMessage"; id: string; text: string }).text = text;
};

describe("ChatView streaming", () => {
  it("does not re-render settled items while the trailing message streams", () => {
    const list = items();
    const { rerender } = render(<ChatView items={list} />);
    expect(calls.tool).toBe(1);

    for (let i = 1; i <= 5; i++) {
      setText(list, "chunk ".repeat(i));
      rerender(<ChatView items={list} />);
    }

    // The settled tool card is behind the streaming message: re-parsing it on
    // every delta is what starves the sidebar spinner's animation frame.
    expect(calls.tool).toBe(1);
  });

  it("still re-renders a tool card whose status changed", () => {
    const list = items();
    // `Session.apply` replaces the slot with a new item object on item/completed.
    // The card sits in the prefix (the streaming agent message is the tail), and
    // an in-place slot replace is invisible to the prefix's identity checks —
    // which is exactly why `Session` bumps `structuralVersion` there. Driving the
    // same pair here is what a real completed tool call looks like to `ChatView`.
    const { getByTestId, rerender } = render(<ChatView items={list} structuralVersion={1} />);
    expect(getByTestId("tool").textContent).toBe("bash");

    list[1] = {
      type: "toolCall",
      id: "tc1",
      call_id: "c1",
      name: "bash",
      input: { cmd: "ls" },
      status: "failed",
      result: "boom",
    };
    rerender(<ChatView items={list} structuralVersion={2} />);
    expect(calls.tool).toBe(2);
  });

  it("keeps the streamed text visible", () => {
    const list = items();
    const { container, rerender } = render(<ChatView items={list} />);
    setText(list, "hello world");
    rerender(<ChatView items={list} />);
    expect(container.textContent).toContain("hello world");
  });

  it("clips horizontal overflow so wide content cannot pan the transcript", () => {
    const { container } = render(<ChatView items={items()} />);
    const scroller = container.firstElementChild as HTMLElement;
    // 只写 `overflow-y-auto` 时，CSS 会把 `overflow-x: visible` 求值成 `auto`：
    // 一旦内容（宽表格/代码块）超出容器，整个会话区就能横向拖动，左移后右边留下
    // 大片空白。显式 `overflow-x-hidden` 才是「内容不得横向溢出」的本意。
    expect(scroller.className).toContain("overflow-y-auto");
    expect(scroller.className).toContain("overflow-x-hidden");
    // 作为 flex 子项，缺 `min-w-0` 时它的最小宽度是内容宽度，同样会撑破布局。
    expect(scroller.className).toContain("min-w-0");
  });
});

describe("ChatView user attachments", () => {
  it("lists the attachments on a user bubble", () => {
    const { getAllByTestId } = render(
      <ChatView
        items={[
          {
            type: "userMessage",
            id: "u1",
            text: "总结一下",
            attachments: [
              { name: "报告.pdf", path: ".yi-agent/attachments/t1/x-报告.pdf", size: 10 },
              { name: "预算.xlsx", path: ".yi-agent/attachments/t1/y-预算.xlsx", size: 20 },
            ],
          },
        ]}
      />,
    );
    expect(screen.getByText("总结一下")).toBeTruthy();
    const chips = getAllByTestId("bubble-attachment");
    expect(chips.map((c) => c.textContent)).toEqual(["报告.pdf", "预算.xlsx"]);
    // 完整路径只作为悬停提示，不进气泡正文。
    expect(chips[0].getAttribute("title")).toBe(".yi-agent/attachments/t1/x-报告.pdf");
  });

  it("renders a user bubble without attachments unchanged", () => {
    const { getByText, queryAllByTestId } = render(
      <ChatView items={[{ type: "userMessage", id: "u2", text: "hi" }]} />,
    );
    expect(getByText("hi")).toBeTruthy();
    expect(queryAllByTestId("bubble-attachment")).toHaveLength(0);
    // 无附件的用户气泡必须与改动前逐字一致。
    const bubble = getByText("hi");
    expect(bubble.className).toBe(
      "my-1 max-w-[80%] self-end rounded-lg bg-blue-600 px-3 py-2 text-sm whitespace-pre-wrap text-white",
    );
    expect(bubble.textContent).toBe("hi");
  });

  it("falls back to the file name when an attachment has no name", () => {
    const { getAllByTestId } = render(
      <ChatView
        items={[
          {
            type: "userMessage",
            id: "u3",
            text: "看看",
            attachments: [{ name: "", path: ".yi-agent/attachments/t1/abc-预算.xlsx", size: 3 }],
          },
        ]}
      />,
    );
    // 不能留一个没有标题的空白 chip：退回路径最后一段。
    const chips = getAllByTestId("bubble-attachment");
    expect(chips.map((c) => c.textContent)).toEqual(["abc-预算.xlsx"]);
  });
});

describe("ChatView user images", () => {
  it("renders an image for each server-echoed ref on a user bubble", () => {
    const { getAllByTestId } = render(
      <ChatView
        items={[
          {
            type: "userMessage",
            id: "u4",
            text: "看这两张",
            images: [
              { path: ".yi-agent/attachments/t1/ab-a.png", media_type: "image/png", size: 3 },
              { path: ".yi-agent/attachments/t1/cd-b.jpg", media_type: "image/jpeg", size: 4 },
            ],
          },
        ]}
      />,
    );
    // 乐观回声只带文档附件；已发送的图片只能靠服务端回声里的 refs 画出来。
    const imgs = getAllByTestId("bubble-image");
    expect(imgs).toHaveLength(2);
    expect(imgs[0].getAttribute("src")).toBe("blob:test-image");
  });

  it("renders no image when a message has no image refs", () => {
    const { queryAllByTestId } = render(
      <ChatView items={[{ type: "userMessage", id: "u5", text: "纯文字" }]} />,
    );
    expect(queryAllByTestId("bubble-image")).toHaveLength(0);
  });
});

describe("ChatView notices", () => {
  it("renders a config notice as a card", () => {
    const list = [
      {
        type: "notice",
        id: "n1",
        text: "",
        body: {
          kind: "config",
          data: { model: "claude-sonnet-4", sandbox: "WorkspaceWrite" },
          error: null,
        },
      },
    ] as unknown as (Item | NoticeItem)[];
    render(<ChatView items={list} />);
    expect(screen.getByTestId("config-card")).toBeTruthy();
    expect(screen.getByText("当前配置")).toBeTruthy();
    expect(screen.getByText("workspace-write")).toBeTruthy();
  });

  it("renders a plain notice as the compact capsule", () => {
    const list = [{ type: "notice", id: "n2", text: "对话已清空" }] as unknown as (
      | Item
      | NoticeItem
    )[];
    render(<ChatView items={list} />);
    expect(screen.getByText("对话已清空")).toBeTruthy();
    expect(screen.queryByTestId("config-card")).toBeNull();
  });

  it("renders the card when the read failed and data is null", () => {
    // 读取失败：data 为 null 但 error 非空——父分支必须只看载荷是否存在，
    // 不能把「data 为空」误判成普通 notice 的胶囊。
    const list = [
      {
        type: "notice",
        id: "n3",
        text: "",
        body: { kind: "config", data: null, error: "boom" },
      },
    ] as unknown as (Item | NoticeItem)[];
    render(<ChatView items={list} />);
    expect(screen.getByTestId("config-card")).toBeTruthy();
    expect(screen.getByText("读取配置失败: boom")).toBeTruthy();
  });

  it("renders a help notice as the help card", () => {
    const list = [
      {
        type: "notice",
        id: "n4",
        text: "",
        body: { kind: "help", target: null, error: null },
      },
    ] as unknown as (Item | NoticeItem)[];
    render(<ChatView items={list} />);
    expect(screen.getByTestId("help-card")).toBeTruthy();
    expect(screen.getByText("可用命令")).toBeTruthy();
    expect(screen.getByText("清空对话上下文")).toBeTruthy();
  });

  it("renders a help notice for one command as its detail card", () => {
    const list = [
      {
        type: "notice",
        id: "n5",
        text: "",
        body: { kind: "help", target: "cost", error: null },
      },
    ] as unknown as (Item | NoticeItem)[];
    render(<ChatView items={list} />);
    expect(screen.getByText("命令 /cost")).toBeTruthy();
    expect(screen.getByText("显示 token 使用量")).toBeTruthy();
  });
});

describe("ChatView reasoning", () => {
  const block = { type: "reasoning", id: "r1", text: "let me think about this" };

  it("renders a dim「思考」block, collapsed by default and expandable", () => {
    render(<ChatView items={[block] as unknown as (Item | NoticeItem)[]} />);
    const el = screen.getByTestId("reasoning-block");
    // dim 前景（fg-subtle）+ 「思考」前缀。
    expect(el.className).toContain("text-fg-subtle");
    const toggle = screen.getByRole("button", { name: /思考/ });
    // 默认折叠：正文不在 DOM 里。
    expect(screen.queryByText("let me think about this")).toBeNull();
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    // 可展开。
    fireEvent.click(toggle);
    expect(screen.getByText("let me think about this")).toBeTruthy();
    expect(screen.getByRole("button", { name: /思考/ }).getAttribute("aria-expanded")).toBe("true");
  });

  it("renders a replayed server thinking item with the same collapsible card", () => {
    // 历史回放（thread/resume / 冷缓存）期没有本地推理块，思考卡片由服务端的
    // `Item::Thinking` 直接驱动——复用同一张折叠卡片，不新建组件。
    const thinking = {
      type: "thinking",
      id: "item-turn1-2",
      text: "历史思考内容",
      redacted: false,
    };
    render(<ChatView items={[thinking] as unknown as (Item | NoticeItem)[]} />);
    const el = screen.getByTestId("reasoning-block");
    expect(el.className).toContain("text-fg-subtle");
    const toggle = screen.getByRole("button", { name: /思考/ });
    expect(screen.queryByText("历史思考内容")).toBeNull();
    fireEvent.click(toggle);
    expect(screen.getByText("历史思考内容")).toBeTruthy();
  });

  it("renders a redacted thinking item as a placeholder, not raw text", () => {
    const thinking = { type: "thinking", id: "th1", text: "", redacted: true };
    render(<ChatView items={[thinking] as unknown as (Item | NoticeItem)[]} />);
    // 占位文案可见且不暴露凭证；空明文不渲染成一坨空白。
    expect(screen.getByTestId("reasoning-block")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /思考/ }));
    expect(screen.getByText("已加密的思考")).toBeTruthy();
  });
});

describe("ChatView server notices", () => {
  const notice = (level: string) => ({ type: "notice", id: "n", text: "一条提示", level });

  it("tones a Warn notice differently from an Info notice", () => {
    render(<ChatView items={[notice("Info")] as unknown as (Item | NoticeItem)[]} />);
    const infoClass = screen.getByTestId("notice").className;
    cleanup();
    render(<ChatView items={[notice("Warn")] as unknown as (Item | NoticeItem)[]} />);
    const warnClass = screen.getByTestId("notice").className;
    expect(warnClass).not.toBe(infoClass);
    expect(warnClass).toContain("amber");
  });
});

describe("ChatView retry banner", () => {
  afterEach(cleanup);

  it("names every retry cause so a capacity fault is not misreported as a stall", () => {
    // The banner is the only place a user learns why the turn is waiting. Each
    // cause must render its own wording: a 429 is not a stall, and an
    // unreachable provider is neither.
    const cases: Array<[Parameters<typeof ChatView>[0]["retrying"], string]> = [
      [{ attempt: 1, max: 3, cause: "idle_stall" }, "Provider stalled"],
      [{ attempt: 2, max: 3, cause: "request_timeout" }, "Provider request timed out"],
      [{ attempt: 1, max: 3, cause: "rate_limited" }, "Provider rate limited (429)"],
      [{ attempt: 1, max: 3, cause: "server_error" }, "Provider server error (5xx)"],
      [{ attempt: 1, max: 3, cause: "network_error" }, "Provider unreachable"],
    ];

    for (const [retrying, expected] of cases) {
      const { container, unmount } = render(<ChatView items={[]} retrying={retrying} />);
      const text = container.textContent ?? "";
      expect(text).toContain(expected);
      expect(text).toContain(`retrying ${retrying!.attempt}/${retrying!.max}`);
      unmount();
    }
  });
});

describe("ChatView frozen prefix", () => {
  it("does not re-render history above the streaming tail", () => {
    const history: Item[] = [];
    for (let i = 0; i < 200; i += 1) {
      history.push({ type: "agentMessage", id: `h${i}`, text: `history ${i}` });
    }
    const tail = { type: "agentMessage", id: "tail", text: "" } as Item;
    const list = [...history, tail];

    const { rerender } = render(<ChatView items={list} />);
    const settled = calls.text;
    expect(settled).toBe(201); // every item rendered once on mount

    // Stream 5 deltas *in place*, exactly like `Session.apply` does.
    for (let i = 1; i <= 5; i += 1) {
      (tail as { text: string }).text = "chunk ".repeat(i);
      rerender(<ChatView items={list} />);
    }

    // Only the tail may have re-rendered. The 200 history items must not have.
    expect(calls.text - settled).toBeLessThanOrEqual(5);
  });

  it("re-renders history when structuralVersion changes", () => {
    const history: Item[] = [
      { type: "agentMessage", id: "h1", text: "old" },
      { type: "agentMessage", id: "tail", text: "streaming" },
    ];
    const { rerender } = render(<ChatView items={history} structuralVersion={1} />);
    const before = calls.text;

    // An `item/completed` replace: same index, new object, new text.
    history[0] = { type: "agentMessage", id: "h1", text: "settled" };
    rerender(<ChatView items={history} structuralVersion={2} />);

    expect(calls.text).toBeGreaterThan(before);
    expect(screen.getByText("settled")).toBeTruthy();
  });

  it("keeps a prefix reasoning block expanded across a structural change", () => {
    // A structural bump must re-render the prefix **in place**, not remount it.
    // `ReasoningBlock`'s expanded flag and `ToolCallCard`'s open flag are local
    // state: remounting silently collapses a CoT the user opened, and re-issues
    // every prefix bubble's image read. Structural bumps are frequent during a
    // turn (`item/started` / `item/completed` / tool events), so a `key` here
    // means such a block can never stay open while the session is live.
    const history = [
      { type: "userMessage", id: "u1", text: "hi" },
      { type: "reasoning", id: "r1", text: "deep thought" },
      { type: "agentMessage", id: "tail", text: "streaming" },
    ] as unknown as (Item | NoticeItem)[];
    const { rerender } = render(<ChatView items={history} structuralVersion={1} />);

    const toggle = screen.getByRole("button", { name: /思考/ });
    fireEvent.click(toggle);
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
    expect(screen.getByText("deep thought")).toBeTruthy();

    // A structural change elsewhere in the prefix: same index, new object —
    // exactly what `Session` does on `item/completed`.
    history[0] = { type: "userMessage", id: "u1", text: "hi again" } as unknown as Item;
    rerender(<ChatView items={history} structuralVersion={2} />);

    expect(screen.getByText("hi again")).toBeTruthy();
    expect(screen.getByRole("button", { name: /思考/ }).getAttribute("aria-expanded")).toBe("true");
    expect(screen.getByText("deep thought")).toBeTruthy();
  });
});

describe("ChatView live tail", () => {
  // The tail is not always the streaming agent message: `Session` pushes a
  // reasoning block at the end when CoT resumes after the answer has started,
  // which displaces the still-streaming message into the frozen prefix. These
  // drive the real `Session` so the shape cannot drift from `apply`.
  const listOf = (s: Session) => s.items as unknown as (Item | NoticeItem)[];

  it("keeps streaming text live when CoT resumes and pushes it into the prefix", () => {
    const s = new Session();
    s.apply({
      method: "item/started",
      params: { thread_id: "t", item: { type: "agentMessage", id: "a1", text: "" } },
    });
    s.apply({ method: "item/delta", params: { thread_id: "t", item_id: "a1", delta: "first " } });
    // CoT resumes after the text started: the block lands at the tail and the
    // streaming agent message is now the second-to-last item.
    s.apply({
      method: "thread/reasoningDelta",
      params: { thread_id: "t", text: "hmm", kind: "Content" },
    });
    expect(s.items.map((i) => i.type)).toEqual(["agentMessage", "reasoning"]);

    const view = (session: Session) => (
      <ChatView items={listOf(session)} structuralVersion={session.structuralVersion} />
    );
    const { rerender, container } = render(view(s));
    expect(container.textContent).toContain("first");

    // A plain append, exactly like `Session.apply` does it.
    s.apply({ method: "item/delta", params: { thread_id: "t", item_id: "a1", delta: "second " } });
    // The append is in place, so nothing `ChatView` compares by identity has
    // moved; `Session` flags the displaced target instead so the prefix rebuilds.
    rerender(view(s));

    // The reply must not freeze just because a CoT block moved in behind it.
    expect(container.textContent).toContain("first second");
  });

  it("keeps appended CoT visible while the reasoning block is the tail", () => {
    const s = new Session();
    s.apply({
      method: "thread/reasoningDelta",
      params: { thread_id: "t", text: "think", kind: "Content" },
    });
    const view = (session: Session) => (
      <ChatView items={listOf(session)} structuralVersion={session.structuralVersion} />
    );
    const { rerender, container } = render(view(s));
    fireEvent.click(screen.getByRole("button", { name: /思考/ })); // expand
    expect(container.textContent).toContain("think");

    s.apply({
      method: "thread/reasoningDelta",
      params: { thread_id: "t", text: " MORE", kind: "Content" },
    });
    rerender(view(s));

    expect(container.textContent).toContain("think MORE");
  });
});

describe("ChatView streaming markdown throttle", () => {
  it("re-parses far fewer times than there are deltas, and still shows the final text", async () => {
    const list: Item[] = [{ type: "agentMessage", id: "a1", text: "" } as Item];
    const { container, rerender } = render(<ChatView items={list} />);
    const settled = calls.text;

    for (let i = 1; i <= 100; i += 1) {
      (list[0] as { text: string }).text = "tok ".repeat(i);
      rerender(<ChatView items={list} />);
    }

    // Far fewer than 100: the window collapses them.
    expect(calls.text - settled).toBeLessThan(50);

    // And the terminal value is visible — a throttle must not swallow it. The
    // trailing edge defers by up to one window, so await it (a synchronous
    // assertion here would fail for ANY correct throttle).
    (list[0] as { text: string }).text = "FINAL";
    rerender(<ChatView items={list} />);
    await waitFor(() => expect(container.textContent).toContain("FINAL"));
  });
});

describe("ChatView tail handover", () => {
  it("does not leak the previous tail's throttled text into the next message", () => {
    // A throttle is stateful; that state must not outlive the message it
    // belongs to. Streaming deltas mutate the tail in place, so the *tail
    // identity* is what changes when a turn ends and the next message starts —
    // the throttle has to restart with it. Sharing one throttle across whoever
    // is the tail makes the new reply render as a copy of the previous one
    // until the old window elapses.
    const first = { type: "agentMessage", id: "a1", text: "" } as Item;
    const list: Item[] = [first];
    const { container, rerender } = render(<ChatView items={list} />);

    (first as { text: string }).text = "answer-one";
    rerender(<ChatView items={list} />);

    // Second turn starts inside the same throttle window.
    const second = { type: "agentMessage", id: "a2", text: "answer-two" } as Item;
    list.push(second);
    rerender(<ChatView items={list} />);

    expect(container.textContent).toContain("answer-two");
    // The previous message stays rendered exactly once, in the frozen prefix.
    expect(container.textContent?.match(/answer-one/g)).toHaveLength(1);
  });
});

describe("ChatView auto-scroll", () => {
  /** jsdom 没有布局引擎：给滚动容器装上可读写的滚动度量。 */
  function installMetrics(
    el: HTMLElement,
    init: { scrollHeight: number; clientHeight: number; scrollTop?: number },
  ) {
    const state = {
      scrollHeight: init.scrollHeight,
      clientHeight: init.clientHeight,
      scrollTop: init.scrollTop ?? 0,
    };
    Object.defineProperty(el, "scrollHeight", {
      configurable: true,
      get: () => state.scrollHeight,
    });
    Object.defineProperty(el, "clientHeight", {
      configurable: true,
      get: () => state.clientHeight,
    });
    Object.defineProperty(el, "scrollTop", {
      configurable: true,
      get: () => state.scrollTop,
      set: (v: number) => {
        state.scrollTop = v;
      },
    });
    return state;
  }

  function setup(init: { scrollTop?: number } = {}) {
    const list = items();
    const { container, rerender } = render(<ChatView items={list} threadId="t1" />);
    const scroller = container.firstElementChild as HTMLElement;
    const state = installMetrics(scroller, {
      scrollHeight: 1000,
      clientHeight: 400,
      scrollTop: init.scrollTop ?? 600,
    });
    fireEvent.scroll(scroller);
    return { list, container, rerender, scroller, state };
  }

  it("leaves the reading position alone while the agent streams", () => {
    // 用户上翻历史时，每来一个 delta 都不能把视口拽回底部——那会让用户
    // 在生成期间反复手动上滑，等于无法浏览上面的内容。
    const { list, rerender, state } = setup({ scrollTop: 100 });
    for (let i = 1; i <= 5; i += 1) {
      setText(list, "chunk ".repeat(i));
      state.scrollHeight = 1000 + i * 100; // content grew below
      rerender(<ChatView items={list} threadId="t1" />);
    }
    expect(state.scrollTop).toBe(100);
  });

  it("keeps following the stream while the user is at the bottom", () => {
    const { list, rerender, state } = setup({ scrollTop: 600 });
    setText(list, "hello");
    state.scrollHeight = 1100;
    rerender(<ChatView items={list} threadId="t1" />);
    expect(state.scrollTop).toBe(1100);
  });

  it("jumps to a new error banner even when the user scrolled up", () => {
    // 错误横幅是一次性事件，用户必须看到；它不能被「上翻历史」吞掉。
    const { list, rerender, state } = setup({ scrollTop: 100 });
    state.scrollHeight = 1040;
    rerender(<ChatView items={list} threadId="t1" error="boom" />);
    expect(state.scrollTop).toBe(1040);
  });

  it("opens a thread with history at the newest content", () => {
    // 首次挂载时元素还没有滚动位置（scrollTop 为 0），贴底意图会被读成 false；
    // 打开会话必须仍然落在最新内容上，而不是让用户自己滑到底。
    const proto = HTMLElement.prototype;
    const state = { scrollTop: 0 };
    Object.defineProperty(proto, "scrollHeight", { configurable: true, get: () => 1000 });
    Object.defineProperty(proto, "clientHeight", { configurable: true, get: () => 400 });
    Object.defineProperty(proto, "scrollTop", {
      configurable: true,
      get: () => state.scrollTop,
      set: (v: number) => {
        state.scrollTop = v;
      },
    });
    try {
      render(<ChatView items={items()} threadId="t1" />);
      expect(state.scrollTop).toBe(1000);
    } finally {
      delete (proto as unknown as Record<string, unknown>).scrollHeight;
      delete (proto as unknown as Record<string, unknown>).clientHeight;
      delete (proto as unknown as Record<string, unknown>).scrollTop;
    }
  });

  it("jumps to the newest content when the thread changes", () => {
    // 切会话是明确意图：要看到这个会话的最新内容，而不是沿用上一个会话
    // 的阅读位置（更不能因为上一个会话上翻过就停在中途）。
    const { list, rerender, state } = setup({ scrollTop: 100 });
    rerender(<ChatView items={list} threadId="t2" />);
    expect(state.scrollTop).toBe(1000);
  });
});
