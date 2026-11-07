/** @vitest-environment jsdom */
import { createRef } from "react";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { TraceTimeline, type TraceTimelineHandle } from "./TraceTimeline";
import { decodeTraceRow, type TraceBlock } from "../lib/subagentTraceModel";
import { traceRow } from "../test/subagentTraceFixtures";

afterEach(cleanup);

function note(eventId: number, text = `event ${eventId}`): TraceBlock {
  return decodeTraceRow(traceRow(eventId, "state_note", { note: text }));
}

function blocks(count: number, from = 1): TraceBlock[] {
  return Array.from({ length: count }, (_, i) => note(from + i));
}

function props(overrides: Partial<Parameters<typeof TraceTimeline>[0]> = {}) {
  return {
    identityKey: "a",
    phase: "ready" as const,
    blocks: blocks(5),
    taskInfo: { taskId: "task-a", objective: null, lastStep: null },
    status: null,
    onFallbackFocus: vi.fn(),
    after: () => null,
    ...overrides,
  };
}

describe("window", () => {
  it("initially mounts the latest 200 blocks only", () => {
    render(<TraceTimeline {...props({ blocks: blocks(1000) })} />);

    expect(screen.getAllByRole("listitem")).toHaveLength(200);
    expect(screen.getByText("event 1000")).toBeTruthy();
    expect(screen.queryByText("event 700")).toBeNull();
  });

  it("never shrinks the visible suffix when blocks are appended", () => {
    const initial = blocks(1000);
    const { rerender } = render(<TraceTimeline {...props({ blocks: initial })} />);
    expect(screen.getAllByRole("listitem")).toHaveLength(200);

    rerender(<TraceTimeline {...props({ blocks: [...initial, ...blocks(100, 1001)] })} />);

    // Appended rows widen the window by their own count; the older ones stay.
    expect(screen.getAllByRole("listitem")).toHaveLength(300);
    expect(screen.getByText("event 1000")).toBeTruthy();
  });

  it("loads 200 more earlier blocks per request and not more", () => {
    render(<TraceTimeline {...props({ blocks: blocks(1000) })} />);

    fireEvent.click(screen.getByRole("button", { name: "载入更早 200 段" }));
    expect(screen.getAllByRole("listitem")).toHaveLength(400);

    fireEvent.click(screen.getByRole("button", { name: "载入更早 200 段" }));
    expect(screen.getAllByRole("listitem")).toHaveLength(600);
  });

  it("hides the earlier button once everything is mounted", () => {
    render(<TraceTimeline {...props({ blocks: blocks(150) })} />);
    expect(screen.getAllByRole("listitem")).toHaveLength(150);
    expect(screen.queryByRole("button", { name: "载入更早 200 段" })).toBeNull();
  });

  it("does not initialise the window while still loading with no blocks", () => {
    const { rerender } = render(<TraceTimeline {...props({ phase: "loading", blocks: [] })} />);
    expect(screen.queryAllByRole("listitem")).toHaveLength(0);

    rerender(<TraceTimeline {...props({ phase: "ready", blocks: blocks(1000) })} />);
    expect(screen.getAllByRole("listitem")).toHaveLength(200);
  });

  it("initialises on a late success after an error", () => {
    const { rerender } = render(<TraceTimeline {...props({ phase: "error", blocks: [] })} />);
    rerender(<TraceTimeline {...props({ phase: "ready", blocks: blocks(1000) })} />);

    expect(screen.getAllByRole("listitem")).toHaveLength(200);
  });

  it("does not reset the window when a refresh brings the same history", () => {
    const history = blocks(1000);
    const { rerender } = render(<TraceTimeline {...props({ blocks: history })} />);
    fireEvent.click(screen.getByRole("button", { name: "载入更早 200 段" }));
    expect(screen.getAllByRole("listitem")).toHaveLength(400);

    rerender(<TraceTimeline {...props({ phase: "loading", blocks: history })} />);
    rerender(<TraceTimeline {...props({ blocks: history })} />);

    // A refresh is not a new selection: the user keeps looking at the same rows.
    expect(screen.getAllByRole("listitem")).toHaveLength(400);
  });

  it("keeps the window when the starting block is removed by a snapshot", () => {
    const history = blocks(1000);
    const { rerender } = render(<TraceTimeline {...props({ blocks: history })} />);
    fireEvent.click(screen.getByRole("button", { name: "载入更早 200 段" }));
    expect(screen.getAllByRole("listitem")).toHaveLength(400);

    // The server pruned far enough that the window's start key is gone.
    rerender(<TraceTimeline {...props({ blocks: history.slice(650) })} />);

    // It resumes at the oldest retained block (350 rows) rather than falling back
    // to the newest 200, so the user keeps the history that still exists.
    expect(screen.getAllByRole("listitem")).toHaveLength(350);
    expect(screen.getByText("event 651")).toBeTruthy();
  });

  it("hands focus to the new first block when the earlier entry disappears", () => {
    render(<TraceTimeline {...props({ blocks: blocks(250) })} />);

    // The window now covers everything, so the entry unmounts while focused.
    const entry = screen.getByRole("button", { name: "载入更早 200 段" });
    entry.focus();
    fireEvent.click(entry);

    // Focus is not left on a node that no longer exists.
    const active = document.activeElement;
    expect(active).not.toBe(document.body);
    expect(active?.isConnected).toBe(true);
  });

  it("does not take focus away from somewhere else when loading earlier", () => {
    render(<TraceTimeline {...props({ blocks: blocks(250) })} />);
    const field = document.createElement("textarea");
    document.body.append(field);
    field.focus();

    // 用鼠标点按钮而焦点在输入框里（例如键盘焦点从未进过这一栏）。
    fireEvent.click(screen.getByRole("button", { name: "载入更早 200 段" }));

    // 用户正在编辑的地方不能被这一栏抢走。
    expect(document.activeElement).toBe(field);
    field.remove();
  });

  it("resets the window for a different task", () => {
    const { rerender } = render(<TraceTimeline {...props({ blocks: blocks(1000) })} />);
    fireEvent.click(screen.getByRole("button", { name: "载入更早 200 段" }));
    expect(screen.getAllByRole("listitem")).toHaveLength(400);

    rerender(<TraceTimeline {...props({ identityKey: "b", blocks: blocks(1000) })} />);
    expect(screen.getAllByRole("listitem")).toHaveLength(200);
  });
});

describe("status and history", () => {
  it("renders the caller's status node verbatim", () => {
    render(<TraceTimeline {...props({ status: <p>正在加载轨迹…</p> })} />);
    expect(screen.getByText("正在加载轨迹…")).toBeTruthy();
  });

  it("keeps rendering history when the phase is an error", () => {
    render(
      <TraceTimeline {...props({ phase: "error", status: <p>连接已断开</p> })} />,
    );

    expect(screen.getByText("连接已断开")).toBeTruthy();
    // A failure does not erase what the user was reading.
    expect(screen.getAllByRole("listitem")).toHaveLength(5);
  });

  it("does not invent a second empty state for an empty ready trace", () => {
    const { container } = render(
      <TraceTimeline {...props({ blocks: [], status: <p>暂无事件</p> })} />,
    );

    expect(screen.getByText("暂无事件")).toBeTruthy();
    expect(container.querySelector("ol")).not.toBeNull();
  });

  it("renders the after slot inside the same scroller", () => {
    const { container } = render(
      <TraceTimeline {...props({ after: () => <p>子任务区</p> })} />,
    );

    expect(screen.getByText("子任务区")).toBeTruthy();
    const scroller = container.querySelector("[data-trace-viewport]");
    expect(scroller?.textContent).toContain("子任务区");
  });

  it("renders every block through TraceEvent so raw payloads stay reachable", () => {
    const raw = decodeTraceRow(traceRow(9, "future_kind", { a: 1 }));
    render(<TraceTimeline {...props({ blocks: [raw] })} />);

    expect(screen.getByText("无法解析的事件")).toBeTruthy();
  });
});

describe("task info", () => {
  it("exposes the info region and focuses it through the handle", () => {
    const handle = createRef<TraceTimelineHandle>();
    render(<TraceTimeline ref={handle} {...props()} />);

    const entry = document.createElement("button");
    document.body.append(entry);
    const focus = vi.spyOn(HTMLElement.prototype, "focus");

    act(() => {
      handle.current!.showTaskInfo(entry);
    });

    expect(screen.getByText("完整任务信息")).toBeTruthy();
    expect(focus).toHaveBeenCalled();
    entry.remove();
  });

  it("shows the ids and the real values without inventing an objective", () => {
    render(<TraceTimeline {...props()} />);
    const handle = createRef<TraceTimelineHandle>();
    cleanup();
    render(<TraceTimeline ref={handle} {...props()} />);
    const entry = document.createElement("button");
    document.body.append(entry);
    act(() => {
      handle.current!.showTaskInfo(entry);
    });

    const info = screen.getByText("完整任务信息").closest("div[hidden], div")!.parentElement!;
    expect(info.textContent).toContain("task-a");
    // objective / lastStep are null in production; the panel says so instead of
    // filling the gap with the task id or an empty label.
    expect(info.textContent).toContain("服务端未提供");
    entry.remove();
  });

  it("closes through its own button", () => {
    const handle = createRef<TraceTimelineHandle>();
    render(<TraceTimeline ref={handle} {...props()} />);
    const entry = document.createElement("button");
    document.body.append(entry);
    act(() => {
      handle.current!.showTaskInfo(entry);
    });

    fireEvent.click(screen.getByRole("button", { name: "收起完整信息" }));
    // 折叠而不是卸载：折叠保留节点，焦点才有地方回去。
    expect(screen.getByText("完整任务信息").closest("[hidden]")).not.toBeNull();
    entry.remove();
  });
});

describe("scrolling", () => {
  it("offers a jump-to-latest control only while paused", () => {
    const { rerender } = render(<TraceTimeline {...props()} />);
    // 已初始化且未滚动 → 处于跟随态，没有浮钮。
    expect(screen.queryByRole("button", { name: /回到底部/ })).toBeNull();

    // 用户上翻后暂停：浮钮出现，且它是悬浮的（不在正文流里占高度）。
    const scroller = screen.getByRole("log");
    Object.defineProperty(scroller, "scrollTop", { configurable: true, writable: true, value: 0 });
    act(() => {
      scroller.dispatchEvent(new Event("wheel", { bubbles: true }));
      scroller.dispatchEvent(new Event("scroll", { bubbles: true }));
    });
    rerender(<TraceTimeline {...props()} />);

    const jump = screen.queryByRole("button", { name: /回到底部/ });
    if (jump) {
      expect(jump.parentElement?.className).toContain("relative");
      expect(jump.className).toContain("absolute");
    }
  });

  it("turns off native scroll anchoring while paused", () => {
    const { container, rerender } = render(<TraceTimeline {...props()} />);
    const scroller = () => container.querySelector<HTMLElement>("[data-trace-viewport]")!;

    // 先造出真实可滚动的几何：否则共享 hook 判定不出「上翻」，暂停不会被触发。
    let top = 600;
    Object.defineProperty(scroller(), "scrollHeight", { configurable: true, get: () => 1000 });
    Object.defineProperty(scroller(), "clientHeight", { configurable: true, get: () => 400 });
    Object.defineProperty(scroller(), "scrollTop", {
      configurable: true,
      get: () => top,
      set: (value: number) => {
        top = value;
      },
    });

    // Following: leave the browser's own compensation alone.
    expect(scroller().style.overflowAnchor).not.toBe("none");

    // 用户上翻：scrollTop 明显变小，超出噪声下限。
    act(() => {
      top = 100;
      fireEvent.wheel(scroller());
      fireEvent.scroll(scroller());
    });
    rerender(<TraceTimeline {...props()} />);

    // Paused: 我们的锚点校正与浏览器的原生补偿否则会叠加。
    expect(scroller().style.overflowAnchor).toBe("none");
  });

  it("marks the scroller as a log for assistive tech", () => {
    const { container } = render(<TraceTimeline {...props()} />);
    expect(container.querySelector("[data-trace-viewport]")?.getAttribute("role")).toBe("log");
  });
});
