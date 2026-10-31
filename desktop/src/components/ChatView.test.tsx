/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach, beforeEach } from "vitest";
import { render, cleanup, screen, fireEvent, waitFor, act } from "@testing-library/react";
import type { Item } from "../lib/protocol";
import type { NoticeItem, ReasoningItem } from "../lib/session";
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

describe("ChatView search highlighting", () => {
  function userItems(): (Item | NoticeItem | ReasoningItem)[] {
    return [{ type: "userMessage", id: "u1", text: "alpha beta alpha" }];
  }

  it("marks hits inside user bubbles", () => {
    const { container } = render(<ChatView items={userItems()} query="alpha" />);
    expect(container.querySelectorAll("mark.search-hit").length).toBe(2);
  });

  it("renders no mark when the query is empty", () => {
    const { container } = render(<ChatView items={userItems()} query="" />);
    expect(container.querySelectorAll("mark.search-hit").length).toBe(0);
  });

  it("keeps user text intact around the mark", () => {
    const { container } = render(<ChatView items={userItems()} query="beta" />);
    expect(container.textContent).toContain("alpha beta alpha");
  });

  it("hands its scrolling element out through scrollerRef", () => {
    // 搜索要读 scrollTop/clientHeight 判断"当前视口"，而真正滚动的是
    // ChatView 内部这个节点，不是 App 外层容器。借不出去就只能永远选中第一个。
    const ref: { current: HTMLDivElement | null } = { current: null };
    const { container } = render(<ChatView items={userItems()} scrollerRef={ref} />);
    expect(ref.current).toBe(container.firstElementChild);
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

  it("renders a usage notice as the usage card", () => {
    const list = [
      {
        type: "notice",
        id: "n5",
        text: "",
        body: {
          kind: "usage",
          report: {
            title: "Token 用量",
            model: "claude-sonnet-4-20250514",
            context: { used: 100, limit: null },
            session: { rows: [{ label: "Input", value: 100 }], isSnapshot: false },
            subagent: null,
            cost: "$0.01",
          },
        },
      },
    ] as unknown as (Item | NoticeItem)[];
    render(<ChatView items={list} />);
    // 结构化载荷才是 /usage 的呈现；落到普通胶囊会把卡片压成一行灰字。
    expect(screen.getByTestId("usage-card")).toBeTruthy();
    expect(screen.queryByTestId("notice")).toBeNull();
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
        body: { kind: "help", target: "usage", error: null },
      },
    ] as unknown as (Item | NoticeItem)[];
    render(<ChatView items={list} />);
    expect(screen.getByText("命令 /usage")).toBeTruthy();
    expect(screen.getByText("查看 token 用量与上下文占用")).toBeTruthy();
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

describe("ChatView item ids", () => {
  it("tags every message with its protocol id so a reading position can be anchored", () => {
    const list = items();
    const { container } = render(<ChatView items={list} threadId="t1" />);
    const ids = Array.from(container.querySelectorAll("[data-item-id]")).map(
      (el) => el.getAttribute("data-item-id"),
    );
    expect(ids).toEqual(list.map((it) => it.id));
  });

  it("keeps the message's own alignment when wrapping it for the id", () => {
    // 锚点节点必须有实际布局盒；内层纵向 flex 保留气泡的 self-end 对齐。
    const { container } = render(<ChatView items={items()} threadId="t1" />);
    const first = container.querySelector("[data-item-id]") as HTMLElement;
    expect(first.className.split(" ")).toEqual(expect.arrayContaining(["flex", "flex-col", "shrink-0"]));
    expect(first.className.split(" ")).not.toContain("contents");
    expect(first.firstElementChild?.className).toContain("self-end");
  });
});

describe("ChatView remembers the reading position", () => {
  // 采集是 rAF 节流的；把它换成 setTimeout 让测试能确定地等到回调跑完。
  beforeEach(() => {
    vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback) => setTimeout(() => cb(0), 0));
    vi.stubGlobal("cancelAnimationFrame", (id: number) => clearTimeout(id));
  });
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  /** jsdom 没有布局：给滚动容器与每条消息装上可读的位置度量。 */
  function installLayout(scroller: HTMLElement, tops: number[], scrollHeight = 3000) {
    const state = { scrollTop: 0 };
    Object.defineProperty(scroller, "scrollHeight", { configurable: true, get: () => scrollHeight });
    Object.defineProperty(scroller, "clientHeight", { configurable: true, get: () => 600 });
    Object.defineProperty(scroller, "scrollTop", {
      configurable: true,
      get: () => state.scrollTop,
      set: (v: number) => {
        state.scrollTop = v;
      },
    });
    const rect = (top: number, height = 0) =>
      ({ top, left: 0, bottom: top + height, right: 0, width: 0, height, x: 0, y: top, toJSON() {} }) as DOMRect;
    scroller.getBoundingClientRect = () => rect(0, 600);
    const nodes = Array.from(scroller.querySelectorAll<HTMLElement>("[data-item-id]"));
    nodes.forEach((node, i) => {
      node.getBoundingClientRect = () => rect(tops[i] - state.scrollTop);
    });
    return state;
  }

  /** 等一拍：让 rAF 桩（setTimeout 0）的回调真正执行完。 */
  const settle = async () => {
    await act(async () => {
      await new Promise((r) => setTimeout(r, 5));
    });
  };

  it("reports the top visible message while the user reads history", async () => {
    const seen: Array<[string, unknown]> = [];
    const list = items();
    const { container } = render(
      <ChatView items={list} threadId="t1" onRememberView={(id, a) => seen.push([id, a])} />,
    );
    const scroller = container.firstElementChild as HTMLElement;
    const state = installLayout(scroller, [0, 1000, 2000]);
    state.scrollTop = 1200;
    fireEvent.scroll(scroller);
    await settle();
    const last = seen[seen.length - 1];
    expect(last?.[0]).toBe("t1");
    expect(last?.[1]).toEqual({ itemId: "tc1", offset: -200 });
  });

  it("does not overwrite a thread's position while its content is still loading", async () => {
    // 切到一条还没加载的会话时 DOM 里没有任何消息；此时若上报 `null`，就等于把
    // 它原来的阅读位置抹成「贴底」，用户再回来时永远落在底部。
    const seen: Array<[string, unknown]> = [];
    const list = items();
    const { container, rerender } = render(
      <ChatView items={list} threadId="t1" onRememberView={(id, a) => seen.push([id, a])} />,
    );
    const scroller = container.firstElementChild as HTMLElement;
    const state = installLayout(scroller, [12, 112, 212]);
    state.scrollTop = 1200;
    fireEvent.scroll(scroller);
    await settle();

    rerender(<ChatView items={[]} threadId="t2" onRememberView={(id, a) => seen.push([id, a])} />);
    await settle();
    expect(seen.filter(([id]) => id === "t2")).toEqual([]);
  });

  it("anchors the first message when the user scrolls to the very top", async () => {
    // 容器有 `py-3`：最顶时首条消息在 +12，旧逻辑返回 null（被当成贴底）。
    const seen: Array<[string, unknown]> = [];
    const { container } = render(
      <ChatView items={items()} threadId="t1" onRememberView={(id, a) => seen.push([id, a])} />,
    );
    const scroller = container.firstElementChild as HTMLElement;
    const state = installLayout(scroller, [12, 112, 212]);
    state.scrollTop = 0;
    fireEvent.scroll(scroller);
    await settle();
    expect(seen[seen.length - 1][1]).toEqual({ itemId: "u1", offset: 12 });
  });

  it("reports null when the user is at the bottom", async () => {
    const seen: Array<[string, unknown]> = [];
    const { container } = render(
      <ChatView items={items()} threadId="t1" onRememberView={(id, a) => seen.push([id, a])} />,
    );
    const scroller = container.firstElementChild as HTMLElement;
    const state = installLayout(scroller, [0, 1000, 2000]);
    state.scrollTop = 2400; // scrollHeight 3000 - clientHeight 600
    fireEvent.scroll(scroller);
    await settle();
    expect(seen[seen.length - 1]).toEqual(["t1", null]);
  });

  it("attributes the last position to the thread that was on screen", async () => {
    // 切换线程时旧内容已被替换，靠"离开前最后记住的那次"归属到旧 id。
    const seen: Array<[string, unknown]> = [];
    const list = items();
    const { container, rerender } = render(
      <ChatView items={list} threadId="t1" onRememberView={(id, a) => seen.push([id, a])} />,
    );
    const scroller = container.firstElementChild as HTMLElement;
    const state = installLayout(scroller, [0, 1000, 2000]);
    state.scrollTop = 1200;
    fireEvent.scroll(scroller);
    await settle();
    rerender(<ChatView items={list} threadId="t2" onRememberView={(id, a) => seen.push([id, a])} />);
    const forT1 = seen.filter(([id]) => id === "t1");
    expect(forT1[forT1.length - 1][1]).toEqual({ itemId: "tc1", offset: -200 });
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

  it("jumps to the newest content when the target thread has no remembered position", () => {
    // 首次进入一条没有记录的会话：仍然落在最新内容上。
    const { list, rerender, state } = setup({ scrollTop: 100 });
    rerender(<ChatView items={list} threadId="t2" anchor={undefined} />);
    expect(state.scrollTop).toBe(1000);
  });

  it("restores the remembered position instead of jumping to the newest content", () => {
    // A → B → A：A 有记录，回来必须停在那条消息上，而不是被拽到底部。
    const list = items();
    const { container, rerender } = render(<ChatView items={list} threadId="t2" />);
    const scroller = container.firstElementChild as HTMLElement;
    const state = installMetrics(scroller, {
      scrollHeight: 3000,
      clientHeight: 400,
      scrollTop: 0,
    });
    const rect = (top: number, height = 0) =>
      ({ top, left: 0, bottom: top + height, right: 0, width: 0, height, x: 0, y: top, toJSON() {} }) as DOMRect;
    scroller.getBoundingClientRect = () => rect(0, 400);
    const rows = Array.from(scroller.querySelectorAll<HTMLElement>("[data-item-id]"));
    const tops = [0, 800, 1600];
    rows.forEach((row, i) => {
      row.getBoundingClientRect = () => rect(tops[i] - state.scrollTop);
    });

    rerender(<ChatView items={list} threadId="t1" anchor={{ itemId: "tc1", offset: -100 }} />);
    // 800 - (-100) = 900
    expect(state.scrollTop).toBe(900);
  });

  it("waits for the transcript before restoring, then anchors the message", () => {
    // 冷会话：切过去的瞬间内容还没到（resume 尚未回包）。此时不能把恢复"用掉"，
    // 否则回包后永远不再对位，用户被留在底部。内容到达时节点是新建的，所以这里
    // 用原型级布局桩，让后建的节点也能量到位置。
    const tops: Record<string, number> = { u1: 0, tc1: 800, a1: 1600 };
    const state = { scrollTop: 0 };
    let scroller: HTMLElement | null = null;
    const rect = (top: number) =>
      ({ top, left: 0, bottom: top, right: 0, width: 0, height: 0, x: 0, y: top, toJSON() {} }) as DOMRect;
    const proto = HTMLElement.prototype as unknown as Record<string, unknown>;
    const saved = {
      rect: proto.getBoundingClientRect,
      scrollHeight: Object.getOwnPropertyDescriptor(HTMLElement.prototype, "scrollHeight"),
      clientHeight: Object.getOwnPropertyDescriptor(HTMLElement.prototype, "clientHeight"),
      scrollTop: Object.getOwnPropertyDescriptor(HTMLElement.prototype, "scrollTop"),
    };
    proto.getBoundingClientRect = function (this: HTMLElement) {
      if (this === scroller) return rect(0);
      return rect((tops[this.dataset.itemId ?? ""] ?? 0) - state.scrollTop);
    };
    Object.defineProperty(HTMLElement.prototype, "scrollHeight", {
      configurable: true,
      get(this: HTMLElement) { return this === scroller ? 3000 : 0; },
    });
    Object.defineProperty(HTMLElement.prototype, "clientHeight", {
      configurable: true,
      get(this: HTMLElement) { return this === scroller ? 400 : 0; },
    });
    Object.defineProperty(HTMLElement.prototype, "scrollTop", {
      configurable: true,
      get(this: HTMLElement) { return this === scroller ? state.scrollTop : 0; },
      set(this: HTMLElement, v: number) { if (this === scroller) state.scrollTop = v; },
    });

    try {
      const list = items();
      const { container, rerender } = render(<ChatView items={list} threadId="t2" />);
      scroller = container.firstElementChild as HTMLElement;

      // 切到 t1，内容还在补齐。
      rerender(<ChatView items={[]} threadId="t1" anchor={{ itemId: "tc1", offset: -100 }} syncing />);
      // 内容到达、补齐结束：必须补上恢复（800 - (-100) = 900）。
      rerender(<ChatView items={list} threadId="t1" anchor={{ itemId: "tc1", offset: -100 }} />);
      expect(state.scrollTop).toBe(900);
    } finally {
      proto.getBoundingClientRect = saved.rect;
      for (const [name, desc] of [
        ["scrollHeight", saved.scrollHeight],
        ["clientHeight", saved.clientHeight],
        ["scrollTop", saved.scrollTop],
      ] as const) {
        if (desc) Object.defineProperty(HTMLElement.prototype, name, desc);
      }
    }
  });

  it("restores after a frame-by-frame replay even though the anchor prop is stable", () => {
    // 生产环境的 `anchorFor()` 返回 Map 里同一个对象：引用恒定。冷会话回放是
    // **分帧**到达的，`anchor` 不会换身份，所以恢复不能只依赖 `anchor` 变化——
    // 必须由"内容到达"驱动，否则永远补不上（实测停在 2424 而非 1200）。
    const tops: Record<string, number> = {};
    const state = { scrollTop: 0, contentH: 0 };
    let scroller: HTMLElement | null = null;
    const ROW = 100;
    const PAD = 12;
    const VIEWPORT = 600;
    const rect = (top: number) =>
      ({ top, left: 0, bottom: top, right: 0, width: 0, height: 0, x: 0, y: top, toJSON() {} }) as DOMRect;
    const proto = HTMLElement.prototype as unknown as Record<string, unknown>;
    const saved = {
      rect: proto.getBoundingClientRect,
      sh: Object.getOwnPropertyDescriptor(HTMLElement.prototype, "scrollHeight"),
      ch: Object.getOwnPropertyDescriptor(HTMLElement.prototype, "clientHeight"),
      st: Object.getOwnPropertyDescriptor(HTMLElement.prototype, "scrollTop"),
    };
    proto.getBoundingClientRect = function (this: HTMLElement) {
      if (this === scroller) return rect(0);
      return rect(PAD - state.scrollTop + (tops[this.dataset.itemId ?? ""] ?? 0));
    };
    Object.defineProperty(HTMLElement.prototype, "scrollHeight", {
      configurable: true,
      get(this: HTMLElement) { return this === scroller ? state.contentH + PAD * 2 : 0; },
    });
    Object.defineProperty(HTMLElement.prototype, "clientHeight", {
      configurable: true,
      get(this: HTMLElement) { return this === scroller ? VIEWPORT : 0; },
    });
    Object.defineProperty(HTMLElement.prototype, "scrollTop", {
      configurable: true,
      get(this: HTMLElement) { return this === scroller ? state.scrollTop : 0; },
      set(this: HTMLElement, v: number) {
        if (this !== scroller) return;
        state.scrollTop = Math.max(0, Math.min(v, state.contentH + PAD * 2 - VIEWPORT));
      },
    });

    try {
      const many = (prefix: string, n: number): Item[] =>
        Array.from({ length: n }, (_, i) => ({ type: "userMessage", id: `${prefix}${i}`, text: "" }));
      const setContent = (list: Item[]) => {
        for (const k of Object.keys(tops)) delete tops[k];
        list.forEach((it, i) => { tops[it.id] = i * ROW; });
        state.contentH = list.length * ROW;
      };
      const A = many("a", 30);
      const B = many("b", 5);
      // 与 `useSessionState` 一致：同一个对象存在 Map 里，读出来引用不变。
      const anchorA = { itemId: "a12", offset: 0 };

      setContent(B);
      const { container, rerender } = render(
        <ChatView items={B} threadId="B" anchor={null} />,
      );
      scroller = container.firstElementChild as HTMLElement;

      // 切回 A：冷，内容还没到，正在补齐。
      rerender(<ChatView items={[]} threadId="A" anchor={anchorA} syncing />);
      // 回放分帧到达（引用始终是 anchorA）。中途某帧可能不含锚点那条。
      for (const n of [10, 20, 30]) {
        const partial = A.slice(0, n);
        setContent(partial);
        rerender(<ChatView items={partial} threadId="A" anchor={anchorA} syncing />);
      }
      // 回放完成。
      setContent(A);
      rerender(<ChatView items={A} threadId="A" anchor={anchorA} />);
      // `offset: 0` = 这条消息顶边贴着视口顶。它的内容坐标是 `py-3`(12) + 1200，
      // 所以恢复出的 scrollTop 是 1212 —— 采集与恢复用的是同一套坐标系，
      // 往返后"顶边在视口顶"这一点必须保持不变。
      expect(state.scrollTop).toBe(1212);
    } finally {
      proto.getBoundingClientRect = saved.rect;
      for (const [name, desc] of [
        ["scrollHeight", saved.sh],
        ["clientHeight", saved.ch],
        ["scrollTop", saved.st],
      ] as const) {
        if (desc) Object.defineProperty(HTMLElement.prototype, name, desc);
      }
    }
  });

  it("re-anchors when content above the anchor grows after the restore", async () => {
    // 恢复的那一刻图片还没加载完（`BubbleImage` 无 url 时返回 null，高度 0），
    // 图片到达后上方内容变高，锚点那条就被推离了视口顶。恢复不能只做一次
    // 像素写入，必须在锚点行**实际位移**后重新对位。
    const tops: Record<string, number> = {};
    const state = { scrollTop: 0, contentH: 0 };
    let scroller: HTMLElement | null = null;
    const PAD = 12;
    const VIEWPORT = 600;
    const rect = (top: number) =>
      ({ top, left: 0, bottom: top, right: 0, width: 0, height: 0, x: 0, y: top, toJSON() {} }) as DOMRect;
    const proto = HTMLElement.prototype as unknown as Record<string, unknown>;
    const saved = {
      rect: proto.getBoundingClientRect,
      sh: Object.getOwnPropertyDescriptor(HTMLElement.prototype, "scrollHeight"),
      ch: Object.getOwnPropertyDescriptor(HTMLElement.prototype, "clientHeight"),
      st: Object.getOwnPropertyDescriptor(HTMLElement.prototype, "scrollTop"),
    };
    proto.getBoundingClientRect = function (this: HTMLElement) {
      if (this === scroller) return rect(0);
      return rect(PAD - state.scrollTop + (tops[this.dataset.itemId ?? ""] ?? 0));
    };
    Object.defineProperty(HTMLElement.prototype, "scrollHeight", {
      configurable: true,
      get(this: HTMLElement) { return this === scroller ? state.contentH + PAD * 2 : 0; },
    });
    Object.defineProperty(HTMLElement.prototype, "clientHeight", {
      configurable: true,
      get(this: HTMLElement) { return this === scroller ? VIEWPORT : 0; },
    });
    Object.defineProperty(HTMLElement.prototype, "scrollTop", {
      configurable: true,
      get(this: HTMLElement) { return this === scroller ? state.scrollTop : 0; },
      set(this: HTMLElement, v: number) {
        if (this !== scroller) return;
        state.scrollTop = Math.max(0, Math.min(v, state.contentH + PAD * 2 - VIEWPORT));
      },
    });

    try {
      const list: Item[] = Array.from({ length: 20 }, (_, i) => ({
        type: "userMessage" as const,
        id: `a${i}`,
        text: `m${i}`,
      }));
      const ROW = 100;
      const rebuild = (firstRowHeight: number) => {
        for (const k of Object.keys(tops)) delete tops[k];
        tops.a0 = 0;
        for (let i = 1; i < 20; i++) tops[`a${i}`] = firstRowHeight + (i - 1) * ROW;
        state.contentH = firstRowHeight + 19 * ROW;
      };
      rebuild(ROW);

      const anchorA = { itemId: "a10", offset: 0 };
      const { container, rerender } = render(<ChatView items={list} threadId="B" anchor={null} />);
      scroller = container.firstElementChild as HTMLElement;

      rerender(<ChatView items={list} threadId="A" anchor={anchorA} />);
      // 首行图片加载完成：高 100 -> 400，锚点上方多出 300px。
      rebuild(400);
      rerender(<ChatView items={list} threadId="A" anchor={anchorA} />);
      // 布局变化是异步可观测的（ResizeObserver / 轮询兜底），等它跑完。
      await act(async () => { await new Promise((r) => setTimeout(r, 150)); });
      // 锚点行顶边必须仍在视口顶。`a10` 的内容坐标 = py-3(12) + 400 + 9*100
      // = 1312；`offset: 0` 即"顶边贴视口顶"，所以恢复出的 scrollTop 就是 1312。
      expect(state.scrollTop).toBe(1312);
    } finally {
      proto.getBoundingClientRect = saved.rect;
      for (const [name, desc] of [
        ["scrollHeight", saved.sh],
        ["clientHeight", saved.ch],
        ["scrollTop", saved.st],
      ] as const) {
        if (desc) Object.defineProperty(HTMLElement.prototype, name, desc);
      }
    }
  });

  it("does not mistake its own scrollTop write for the user taking over", async () => {
    // 真实浏览器在**程序化**写入 `scrollTop` 之后也会异步派发 `scroll` 事件。
    // 把它当成"用户接管"，漂移校正从恢复那一刻起就永久失效：锚点上方图片加载
    // 变高后，锚点行被推下去而没人再管（实测停在 1012，应为 1312）。
    const tops: Record<string, number> = {};
    const state = { scrollTop: 0, contentH: 0 };
    let scroller: HTMLElement | null = null;
    const PAD = 12;
    const VIEWPORT = 600;
    const ROW = 100;
    const rect = (top: number) =>
      ({ top, left: 0, bottom: top, right: 0, width: 0, height: 0, x: 0, y: top, toJSON() {} }) as DOMRect;
    const proto = HTMLElement.prototype as unknown as Record<string, unknown>;
    const saved = {
      rect: proto.getBoundingClientRect,
      sh: Object.getOwnPropertyDescriptor(HTMLElement.prototype, "scrollHeight"),
      ch: Object.getOwnPropertyDescriptor(HTMLElement.prototype, "clientHeight"),
      st: Object.getOwnPropertyDescriptor(HTMLElement.prototype, "scrollTop"),
    };
    proto.getBoundingClientRect = function (this: HTMLElement) {
      if (this === scroller) return rect(0);
      return rect(PAD - state.scrollTop + (tops[this.dataset.itemId ?? ""] ?? 0));
    };
    Object.defineProperty(HTMLElement.prototype, "scrollHeight", {
      configurable: true,
      get(this: HTMLElement) { return this === scroller ? state.contentH + PAD * 2 : 0; },
    });
    Object.defineProperty(HTMLElement.prototype, "clientHeight", {
      configurable: true,
      get(this: HTMLElement) { return this === scroller ? VIEWPORT : 0; },
    });
    Object.defineProperty(HTMLElement.prototype, "scrollTop", {
      configurable: true,
      get(this: HTMLElement) { return this === scroller ? state.scrollTop : 0; },
      set(this: HTMLElement, v: number) {
        if (this !== scroller) return;
        state.scrollTop = Math.max(0, Math.min(v, state.contentH + PAD * 2 - VIEWPORT));
        // 浏览器在下一次渲染的 scroll steps 派发，不保证早于 0ms 定时器。
        setTimeout(() => { scroller?.dispatchEvent(new Event("scroll")); }, 16);
      },
    });

    try {
      const list: Item[] = Array.from({ length: 20 }, (_, i) => ({
        type: "userMessage" as const,
        id: `a${i}`,
        text: `m${i}`,
      }));
      const rebuild = (firstRowHeight: number) => {
        for (const k of Object.keys(tops)) delete tops[k];
        tops.a0 = 0;
        for (let i = 1; i < 20; i++) tops[`a${i}`] = firstRowHeight + (i - 1) * ROW;
        state.contentH = firstRowHeight + 19 * ROW;
      };
      rebuild(ROW);

      const anchorA = { itemId: "a10", offset: 0 };
      const { container, rerender } = render(<ChatView items={list} threadId="B" anchor={null} />);
      scroller = container.firstElementChild as HTMLElement;
      rerender(<ChatView items={list} threadId="A" anchor={anchorA} />);
      await act(async () => { await new Promise((r) => setTimeout(r, 30)); });

      rebuild(400);
      rerender(<ChatView items={list} threadId="A" anchor={anchorA} />);
      await act(async () => { await new Promise((r) => setTimeout(r, 200)); });
      expect(state.scrollTop).toBe(PAD + 400 + 9 * ROW);
    } finally {
      proto.getBoundingClientRect = saved.rect;
      for (const [name, desc] of [
        ["scrollHeight", saved.sh],
        ["clientHeight", saved.ch],
        ["scrollTop", saved.st],
      ] as const) {
        if (desc) Object.defineProperty(HTMLElement.prototype, name, desc);
      }
    }
  });

  it("hands control back to the user once they scroll after a restore", async () => {
    // 漂移校正只能在用户还没动的时候生效；否则用户每滚一下就被拽回去。
    const tops: Record<string, number> = {};
    const state = { scrollTop: 0, contentH: 0 };
    let scroller: HTMLElement | null = null;
    const PAD = 12;
    const VIEWPORT = 600;
    const rect = (top: number) =>
      ({ top, left: 0, bottom: top, right: 0, width: 0, height: 0, x: 0, y: top, toJSON() {} }) as DOMRect;
    const proto = HTMLElement.prototype as unknown as Record<string, unknown>;
    const saved = {
      rect: proto.getBoundingClientRect,
      sh: Object.getOwnPropertyDescriptor(HTMLElement.prototype, "scrollHeight"),
      ch: Object.getOwnPropertyDescriptor(HTMLElement.prototype, "clientHeight"),
      st: Object.getOwnPropertyDescriptor(HTMLElement.prototype, "scrollTop"),
    };
    proto.getBoundingClientRect = function (this: HTMLElement) {
      if (this === scroller) return rect(0);
      return rect(PAD - state.scrollTop + (tops[this.dataset.itemId ?? ""] ?? 0));
    };
    Object.defineProperty(HTMLElement.prototype, "scrollHeight", {
      configurable: true,
      get(this: HTMLElement) { return this === scroller ? state.contentH + PAD * 2 : 0; },
    });
    Object.defineProperty(HTMLElement.prototype, "clientHeight", {
      configurable: true,
      get(this: HTMLElement) { return this === scroller ? VIEWPORT : 0; },
    });
    Object.defineProperty(HTMLElement.prototype, "scrollTop", {
      configurable: true,
      get(this: HTMLElement) { return this === scroller ? state.scrollTop : 0; },
      set(this: HTMLElement, v: number) {
        if (this !== scroller) return;
        state.scrollTop = Math.max(0, Math.min(v, state.contentH + PAD * 2 - VIEWPORT));
      },
    });

    try {
      const list: Item[] = Array.from({ length: 20 }, (_, i) => ({
        type: "userMessage" as const,
        id: `a${i}`,
        text: `m${i}`,
      }));
      const ROW = 100;
      const rebuild = (firstRowHeight: number) => {
        for (const k of Object.keys(tops)) delete tops[k];
        tops.a0 = 0;
        for (let i = 1; i < 20; i++) tops[`a${i}`] = firstRowHeight + (i - 1) * ROW;
        state.contentH = firstRowHeight + 19 * ROW;
      };
      rebuild(ROW);

      const anchorA = { itemId: "a10", offset: 0 };
      const { container, rerender } = render(<ChatView items={list} threadId="B" anchor={null} />);
      scroller = container.firstElementChild as HTMLElement;
      rerender(<ChatView items={list} threadId="A" anchor={anchorA} />);

      // 用户自己往上翻。
      state.scrollTop = 500;
      fireEvent.scroll(scroller);
      // 接着上方内容变高：校正必须让位。
      rebuild(400);
      rerender(<ChatView items={list} threadId="A" anchor={anchorA} />);
      await act(async () => { await new Promise((r) => setTimeout(r, 150)); });
      expect(state.scrollTop).toBe(500);
    } finally {
      proto.getBoundingClientRect = saved.rect;
      for (const [name, desc] of [
        ["scrollHeight", saved.sh],
        ["clientHeight", saved.ch],
        ["scrollTop", saved.st],
      ] as const) {
        if (desc) Object.defineProperty(HTMLElement.prototype, name, desc);
      }
    }
  });

  it("falls back to the bottom when the anchored message is gone", () => {
    const list = items();
    const { container, rerender } = render(<ChatView items={list} threadId="t2" />);
    const scroller = container.firstElementChild as HTMLElement;
    const state = installMetrics(scroller, { scrollHeight: 2000, clientHeight: 400, scrollTop: 0 });
    rerender(
      <ChatView
        items={list}
        threadId="t1"
        anchor={{ itemId: "item-that-was-deleted", offset: 0 }}
      />,
    );
    expect(state.scrollTop).toBe(2000);
  });

  it("does not follow new content once a restored position is on screen", () => {
    // 恢复后必须放弃跟随；否则下一条 delta 就把人拽回底部。
    const list = items();
    const { container, rerender } = render(<ChatView items={list} threadId="t2" />);
    const scroller = container.firstElementChild as HTMLElement;
    const state = installMetrics(scroller, { scrollHeight: 3000, clientHeight: 400, scrollTop: 0 });
    const rect = (top: number, height = 0) =>
      ({ top, left: 0, bottom: top + height, right: 0, width: 0, height, x: 0, y: top, toJSON() {} }) as DOMRect;
    scroller.getBoundingClientRect = () => rect(0, 400);
    const rows = Array.from(scroller.querySelectorAll<HTMLElement>("[data-item-id]"));
    const tops = [0, 800, 1600];
    rows.forEach((row, i) => {
      row.getBoundingClientRect = () => rect(tops[i] - state.scrollTop);
    });
    rerender(<ChatView items={list} threadId="t1" anchor={{ itemId: "tc1", offset: -100 }} />);
    expect(state.scrollTop).toBe(900);
    setText(list, "new delta");
    state.scrollHeight = 3400;
    rerender(<ChatView items={list} threadId="t1" anchor={{ itemId: "tc1", offset: -100 }} />);
    expect(state.scrollTop).toBe(900);
  });
  it("resets user takeover when returning to the same thread", async () => {
    const list: Item[] = Array.from({ length: 20 }, (_, i) => ({
      type: "userMessage" as const,
      id: `a${i}`,
      text: `m${i}`,
    }));
    const tops: Record<string, number> = {};
    const state = { scrollTop: 0, contentH: 2000 };
    let scroller: HTMLElement | null = null;
    const rect = (top: number) =>
      ({ top, left: 0, bottom: top, right: 0, width: 0, height: 0, x: 0, y: top, toJSON() {} }) as DOMRect;
    const proto = HTMLElement.prototype as unknown as Record<string, unknown>;
    const saved = {
      rect: proto.getBoundingClientRect,
      sh: Object.getOwnPropertyDescriptor(HTMLElement.prototype, "scrollHeight"),
      ch: Object.getOwnPropertyDescriptor(HTMLElement.prototype, "clientHeight"),
      st: Object.getOwnPropertyDescriptor(HTMLElement.prototype, "scrollTop"),
    };
    proto.getBoundingClientRect = function (this: HTMLElement) {
      if (this === scroller) return rect(0);
      return rect(12 + (tops[this.dataset.itemId ?? ""] ?? 0) - state.scrollTop);
    };
    Object.defineProperty(HTMLElement.prototype, "scrollHeight", {
      configurable: true,
      get(this: HTMLElement) { return this === scroller ? state.contentH + 24 : 0; },
    });
    Object.defineProperty(HTMLElement.prototype, "clientHeight", {
      configurable: true,
      get(this: HTMLElement) { return this === scroller ? 600 : 0; },
    });
    Object.defineProperty(HTMLElement.prototype, "scrollTop", {
      configurable: true,
      get(this: HTMLElement) { return this === scroller ? state.scrollTop : 0; },
      set(this: HTMLElement, value: number) { if (this === scroller) state.scrollTop = value; },
    });

    try {
      tops.a0 = 0;
      for (let i = 1; i < list.length; i++) tops[`a${i}`] = i * 100;
      const anchorA = { itemId: "a10", offset: 0 };
      const { container, rerender } = render(
        <ChatView items={list} threadId="A" anchor={anchorA} />,
      );
      scroller = container.firstElementChild as HTMLElement;
      // A 已恢复，用户主动滚动，随后切 B 再返回 A。
      state.scrollTop = 500;
      fireEvent.scroll(scroller);
      rerender(<ChatView items={list} threadId="B" anchor={null} />);
      rerender(<ChatView items={list} threadId="A" anchor={anchorA} />);
      expect(state.scrollTop).toBe(1012);

      // A 中锚点上方的异步内容变高；用户回到 A 后应重新得到自动校正。
      for (let i = 1; i < list.length; i++) tops[`a${i}`] = i * 100 + 400;
      state.contentH += 400;
      await act(async () => { await new Promise((resolve) => setTimeout(resolve, 150)); });
      expect(state.scrollTop).toBe(1412);
    } finally {
      proto.getBoundingClientRect = saved.rect;
      for (const [name, descriptor] of [
        ["scrollHeight", saved.sh],
        ["clientHeight", saved.ch],
        ["scrollTop", saved.st],
      ] as const) {
        if (descriptor) Object.defineProperty(HTMLElement.prototype, name, descriptor);
      }
    }
  });
});

describe("ChatView keeps reporting 'bottom' while the stream grows", () => {
  beforeEach(() => {
    vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback) => setTimeout(() => cb(0), 0));
    vi.stubGlobal("cancelAnimationFrame", (id: number) => clearTimeout(id));
  });
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("does not remember an anchor just because content grew before the capture frame", async () => {
    // 真实浏览器里 `scroll` 事件是异步派发的：我们贴底之后内容又长高了，采集那一帧
    // 读到的绝对 gap 早就超过阈值。用当下 gap 判定就会把「仍在底部」记成「在读历史」，
    // 切走再回来时被恢复到一个消息锚点，随后 releaseFollow 让跟随永久停掉。
    const seen: Array<[string, unknown]> = [];
    const list = items();
    const { container } = render(
      <ChatView items={list} threadId="t1" onRememberView={(id, a) => seen.push([id, a])} />,
    );
    const scroller = container.firstElementChild as HTMLElement;
    const state = { scrollTop: 0, scrollHeight: 3000 };
    Object.defineProperty(scroller, "scrollHeight", {
      configurable: true,
      get: () => state.scrollHeight,
    });
    Object.defineProperty(scroller, "clientHeight", { configurable: true, get: () => 600 });
    Object.defineProperty(scroller, "scrollTop", {
      configurable: true,
      get: () => state.scrollTop,
      set: (v: number) => {
        state.scrollTop = v;
      },
    });
    const rect = (top: number, height = 0) =>
      ({ top, left: 0, bottom: top + height, right: 0, width: 0, height, x: 0, y: top, toJSON() {} }) as DOMRect;
    scroller.getBoundingClientRect = () => rect(0, 600);
    Array.from(scroller.querySelectorAll<HTMLElement>("[data-item-id]")).forEach((node, i) => {
      node.getBoundingClientRect = () => rect([0, 1000, 2000][i] - state.scrollTop);
    });

    // 用户仍在底部：scrollTop 是当前最大值 2400。
    state.scrollTop = 2400;
    fireEvent.scroll(scroller);
    // 采集帧还没跑，内容先长高了 800px（流式 delta）。
    state.scrollHeight = 3800;
    await act(async () => {
      await new Promise((r) => setTimeout(r, 5));
    });

    // 视口一行都没动，仍在底部 ⇒ 必须记成 null（贴底），而不是某条消息的锚点。
    expect(seen[seen.length - 1]).toEqual(["t1", null]);
  });
});
