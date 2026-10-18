/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, cleanup, screen, fireEvent } from "@testing-library/react";
import type { Item } from "../lib/protocol";
import type { NoticeItem } from "../lib/session";

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
        config: { data: { model: "claude-sonnet-4", sandbox: "WorkspaceWrite" } },
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
    // 读取失败：data 为 null 但 error 非空——父分支必须只看 config 是否存在，
    // 不能把「data 为空」误判成普通 notice 的胶囊。
    const list = [
      { type: "notice", id: "n3", text: "", config: { data: null, error: "boom" } },
    ] as unknown as (Item | NoticeItem)[];
    render(<ChatView items={list} />);
    expect(screen.getByTestId("config-card")).toBeTruthy();
    expect(screen.getByText("读取配置失败: boom")).toBeTruthy();
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
});
