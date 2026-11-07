/** @vitest-environment jsdom */
import { useState } from "react";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";

import { TraceEvent, type TracePreserve } from "./TraceEvent";
import { decodeTraceRow } from "../lib/subagentTraceModel";
import { traceRow } from "../test/subagentTraceFixtures";

afterEach(cleanup);

function blockOf(eventId: number, kind: string, payload: unknown) {
  return decodeTraceRow(traceRow(eventId, kind, payload));
}

describe("assistant text", () => {
  it("renders markdown instead of literal asterisks", () => {
    const block = blockOf(1, "assistant_text", { text: "**bold** and `code`" });
    render(<TraceEvent block={block} />);

    expect(screen.getByText("bold").tagName).toBe("STRONG");
    expect(screen.getByText("code").tagName).toBe("CODE");
  });

  it("wraps prose so a long token cannot widen the panel", () => {
    const block = blockOf(1, "assistant_text", { text: "x".repeat(300) });
    const { container } = render(<TraceEvent block={block} />);

    expect(container.querySelector(".trace-prose")).not.toBeNull();
  });
});

describe("tool calls", () => {
  it("shows the tool name apart from its argument summary", () => {
    const block = blockOf(2, "tool_call", { name: "bash", summary: "cargo test -p core" });
    render(<TraceEvent block={block} />);

    expect(screen.getByText("bash")).toBeTruthy();
    expect(screen.getByText("cargo test -p core")).toBeTruthy();
  });

  it("keeps an unknown tool's name and does not invent one", () => {
    const block = blockOf(3, "tool_call", { name: "brand_new_tool", summary: "did something" });
    render(<TraceEvent block={block} />);

    expect(screen.getByText("brand_new_tool")).toBeTruthy();
  });
});

describe("tool results", () => {
  const long = {
    name: "read",
    summary: "first\nsecond\nthird\nfourth",
    is_error: false,
  };

  it("calls a result a result and labels it with its own tool name", () => {
    const block = blockOf(4, "tool_result", long);
    render(<TraceEvent block={block} />);

    // 「结果摘要」是唯一允许的说法：客户端不知道它与哪次调用配对。
    expect(screen.getByText("结果摘要")).toBeTruthy();
    expect(screen.getByText("read")).toBeTruthy();
    expect(screen.queryByText(/输出/)).toBeNull();
  });

  it("folds a long result behind a button and can open it", () => {
    const block = blockOf(4, "tool_result", long);
    render(<TraceEvent block={block} />);

    expect(screen.queryByText(/fourth/)).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: /展开结果摘要/ }));
    expect(screen.getByText(/fourth/)).toBeTruthy();
    expect(screen.getByRole("button", { name: /收起结果摘要/ }).getAttribute("aria-expanded")).toBe("true");
  });

  it("keeps a finalised result expanded when another block arrives", () => {
    const block = blockOf(3, "tool_result", long);
    const { rerender } = render(<TraceEvent block={block} />);
    fireEvent.click(screen.getByRole("button", { name: /展开结果摘要/ }));
    const before = screen.getByText(/fourth/);

    // The parent re-renders with a new object for the same event: the user's
    // reading position must not collapse because a frame arrived.
    rerender(<TraceEvent block={{ ...block }} />);

    expect(screen.getByText(/fourth/)).toBe(before);
    expect(screen.getByRole("button", { name: /收起结果摘要/ }).getAttribute("aria-expanded")).toBe("true");
  });

  it("marks an error result and keeps its first line visible", () => {
    const block = blockOf(5, "tool_result", {
      name: "bash",
      summary: "command failed: ENOENT\nstack line 2\nstack line 3\nstack line 4",
      is_error: true,
    });
    render(<TraceEvent block={block} />);

    expect(screen.getByText(/command failed: ENOENT/)).toBeTruthy();
  });

  it("says a successful result was empty instead of showing a blank box", () => {
    const block = blockOf(6, "tool_result", { name: "read", summary: "", is_error: false });
    render(<TraceEvent block={block} />);

    expect(screen.getByText("（无输出）")).toBeTruthy();
  });

  it("says an empty error result is an error rather than silently empty", () => {
    const block = blockOf(7, "tool_result", { name: "bash", summary: "", is_error: true });
    render(<TraceEvent block={block} />);

    expect(screen.getByText(/失败且无输出/)).toBeTruthy();
  });

  it("keeps the expansion when the same block's text grows", () => {
    const block = blockOf(18, "tool_result", {
      name: "read", summary: "1\n2\n3\n4", is_error: false,
    });
    const { rerender } = render(<TraceEvent block={block} />);
    fireEvent.click(screen.getByRole("button", { name: /展开结果摘要/ }));

    // Same event, longer summary (a live row that grew): re-mounting here would
    // collapse what the user just opened.
    rerender(<TraceEvent block={{ ...block, text: "1\n2\n3\n4\n5" }} />);

    expect(screen.getByRole("button", { name: /收起结果摘要/ })).toBeTruthy();
    expect(screen.getByText(/5/)).toBeTruthy();
  });

  it("says the expanded text is the server's summary, not the full output", () => {
    const block = blockOf(8, "tool_result", long);
    render(<TraceEvent block={block} />);
    fireEvent.click(screen.getByRole("button", { name: /展开结果摘要/ }));

    // 客户端拿不到完整输出，只有服务端给的一段摘要；展开后必须说清楚，
    // 否则用户会以为「展开」看到的就是全部。
    expect(screen.getByText(/这是服务端提供的摘要，不是完整输出/)).toBeTruthy();
  });
});

describe("state notes and raw payloads", () => {
  it("renders a state note as a quiet line, not as an assistant message", () => {
    const block = blockOf(9, "state_note", { note: "waiting for permission" });
    const { container } = render(<TraceEvent block={block} />);

    expect(screen.getByText("waiting for permission")).toBeTruthy();
    expect(container.querySelector(".prose")).toBeNull();
  });

  it("keeps an unknown event reachable as its original text", () => {
    const block = blockOf(10, "quantum_entangled", { whatever: [1, 2] });
    render(<TraceEvent block={block} />);

    expect(screen.getByText(/无法解析的事件/)).toBeTruthy();
    expect(screen.getByText(/quantum_entangled/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /查看原始载荷/ }));
    expect(screen.getByText(/\{"whatever":\[1,2\]\}/)).toBeTruthy();
  });

  it("never executes HTML from a raw payload", () => {
    const block = blockOf(11, "some_future_kind", {
      text: "<img src=x onerror=alert(1)>",
    });
    const { container } = render(<TraceEvent block={block} />);
    fireEvent.click(screen.getByRole("button", { name: /查看原始载荷/ }));

    expect(container.querySelector("img")).toBeNull();
    expect(screen.getByText(/onerror/)).toBeTruthy();
  });

  it("renders a decoded block's text as markdown only for assistant text", () => {
    const note = blockOf(12, "state_note", { note: "**not bold**" });
    render(<TraceEvent block={note} />);

    // 状态说明是日志行，不是 Markdown 文档。
    expect(screen.getByText("**not bold**")).toBeTruthy();
  });
});

describe("preserving the reading position", () => {
  it("runs the change inside onPreserve so the caller can anchor first", () => {
    const order: string[] = [];
    const preserve: TracePreserve = (change) => {
      order.push("before");
      change();
      order.push("after");
    };
    const block = blockOf(13, "tool_result", {
      name: "read", summary: "a\nb\nc\nd", is_error: false,
    });
    render(<TraceEvent block={block} onPreserve={preserve} />);
    fireEvent.click(screen.getByRole("button", { name: /展开结果摘要/ }));

    expect(order).toEqual(["before", "after"]);
    // 展开是 onPreserve 的 change() 里做的；文本被 <pre> 包住且归一化过空白。
    expect(screen.getByRole("button", { name: /收起结果摘要/ })).toBeTruthy();
  });

  it("runs a plain change when no preserve hook is given", () => {
    const block = blockOf(14, "tool_result", {
      name: "read", summary: "a\nb\nc\nd", is_error: false,
    });
    render(<TraceEvent block={block} />);
    fireEvent.click(screen.getByRole("button", { name: /展开结果摘要/ }));

    expect(screen.getByRole("button", { name: /收起结果摘要/ })).toBeTruthy();
  });
});

describe("re-render stability", () => {
  it("does not re-mount a block when only its identity changes", () => {
    const block = blockOf(15, "assistant_text", { text: "hello" });
    const { rerender } = render(<TraceEvent block={block} />);
    const node = screen.getByText("hello");
    rerender(<TraceEvent block={{ ...block }} />);

    expect(screen.getByText("hello")).toBe(node);
  });

  it("updates the rendered text when a streaming block grows", () => {
    const block = blockOf(16, "assistant_text", { text: "part one" });
    const { rerender } = render(<TraceEvent block={block} />);
    rerender(<TraceEvent block={{ ...block, text: "part one and two" }} />);

    expect(screen.getByText("part one and two")).toBeTruthy();
  });

  it("keeps a local expansion when the parent re-renders with a stable callback", () => {
    function Host() {
      const [count, setCount] = useState(0);
      const block = blockOf(17, "tool_result", {
        name: "read", summary: "1\n2\n3\n4", is_error: false,
      });
      return (
        <>
          <button type="button" onClick={() => setCount(count + 1)}>
            parent {count}
          </button>
          <TraceEvent block={block} onPreserve={(change) => change()} />
        </>
      );
    }
    render(<Host />);
    fireEvent.click(screen.getByRole("button", { name: /展开结果摘要/ }));
    fireEvent.click(screen.getByRole("button", { name: /^parent/ }));

    expect(screen.getByRole("button", { name: /收起结果摘要/ })).toBeTruthy();
  });
});
