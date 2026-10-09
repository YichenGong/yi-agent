/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, cleanup, screen } from "@testing-library/react";
import type { Item } from "../lib/protocol";

// Count how often the settled tool card's component body runs. A streamed delta
// on the trailing agent message must not touch items that cannot have changed.
const calls = vi.hoisted(() => ({ tool: 0 }));

vi.mock("./ToolCallCard", () => ({
  ToolCallCard: ({ item }: { item: { name: string } }) => {
    calls.tool += 1;
    return <div data-testid="tool">{item.name}</div>;
  },
}));

vi.mock("./MarkdownText", () => ({
  AgentMessage: ({ text }: { text: string }) => <div>{text}</div>,
}));

// 图片字节由 `image/read` 另取（见 useImageData），此处只验证气泡把 hook 给出的
// URL 画成一个 <img>。真实的分片/缓存行为在 useImageData.test.ts 里测。
vi.mock("../lib/useImageData", () => ({
  useImageData: () => ({ url: "blob:test-image", error: null }),
}));

import { ChatView } from "./ChatView";

// jsdom has no layout, so the auto-scroll effect needs a stub.
(Element.prototype as unknown as { scrollIntoView: () => void }).scrollIntoView = () => {};

afterEach(() => {
  cleanup();
  calls.tool = 0;
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
    const { getByTestId, rerender } = render(<ChatView items={list} />);
    expect(getByTestId("tool").textContent).toBe("bash");

    // `Session.apply` replaces the slot with a new item object on item/completed.
    list[1] = {
      type: "toolCall",
      id: "tc1",
      call_id: "c1",
      name: "bash",
      input: { cmd: "ls" },
      status: "failed",
      result: "boom",
    };
    rerender(<ChatView items={list} />);
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
