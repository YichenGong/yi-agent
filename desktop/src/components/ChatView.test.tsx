/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, cleanup } from "@testing-library/react";
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
});
