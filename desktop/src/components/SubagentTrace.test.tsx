/** @vitest-environment jsdom */
/**
 * 子 agent 详情：进入即时间线，底部动作常驻。
 *
 * 这个组件只做**组合**：头部（导航 + 摘要 + 完整信息入口）、时间线、尾部状态、
 * 底部消息区。所有状态事实来自 controller 的单一快照，组件自己不推断运行中，
 * 也不自造第二套空态——否则同一个面板会给出互相矛盾的说法。
 */
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { SubagentTrace } from "./SubagentTrace";
import type { TraceActionSnapshot } from "../lib/subagentTraceActions";
import type { TraceSubscriptionSnapshot } from "../lib/subagentTraceSubscription";
import type { SubagentRow } from "../lib/subagents";
import { traceRow } from "../test/subagentTraceFixtures";

afterEach(cleanup);

function row(overrides: Partial<SubagentRow> = {}): SubagentRow {
  return {
    taskId: "task-a",
    objective: "整理子 agent 的轨迹",
    state: "running",
    lastStep: "写入 TraceTimeline",
    finished: false,
    parentTaskId: null,
    ...overrides,
  };
}

function subscription(
  overrides: Partial<TraceSubscriptionSnapshot> = {},
): TraceSubscriptionSnapshot {
  return {
    owner: null,
    phase: "ready",
    rows: [traceRow(1, "assistant_text", { text: "first line" })],
    error: null,
    disconnected: false,
    waitingForPriorRequest: false,
    ...overrides,
  };
}

function actions(overrides: Partial<TraceActionSnapshot> = {}): TraceActionSnapshot {
  return {
    draft: "",
    draftRevision: 0,
    messagePending: false,
    previewPending: false,
    confirmPending: false,
    confirmation: null,
    error: null,
    notice: null,
    ...overrides,
  };
}

function props(overrides: Partial<Parameters<typeof SubagentTrace>[0]> = {}) {
  return {
    identityKey: "a",
    taskId: "task-a",
    row: row(),
    parent: null,
    children: [],
    subscription: subscription(),
    actions: actions(),
    onDrill: vi.fn(),
    onBack: vi.fn(),
    onRetry: vi.fn(),
    onDraftChange: vi.fn(),
    onSend: vi.fn().mockResolvedValue(undefined),
    onPreview: vi.fn().mockResolvedValue(undefined),
    onConfirm: vi.fn().mockResolvedValue(undefined),
    onDismiss: vi.fn(),
    ...overrides,
  };
}

describe("entry", () => {
  it("shows the timeline immediately instead of a whole-trace expansion gate", () => {
    render(<SubagentTrace {...props()} />);

    expect(screen.getByText("first line")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "展开轨迹" })).toBeNull();
    expect(screen.getByLabelText("向这个 agent 发送消息")).toBeTruthy();
  });

  it("focuses the heading on entry so keyboard users land in the panel", () => {
    render(<SubagentTrace {...props()} />);

    const heading = screen.getByRole("heading", { level: 2 });
    expect(heading.getAttribute("tabindex")).toBe("-1");
    expect(document.activeElement).toBe(heading);
  });

  it("shows the short id in the header and the full one only on demand", () => {
    const long = "task-0123456789abcdef0123456789abcdef";
    render(<SubagentTrace {...props({ taskId: long, row: row({ taskId: long }) })} />);

    const header = screen.getByRole("banner");
    expect(header.textContent).not.toContain(long);
    expect(header.textContent).toContain("task-012");

    fireEvent.click(screen.getByRole("button", { name: "查看完整任务信息" }));
    expect(screen.getByText(long)).toBeTruthy();
  });

  it("does not invent an objective or a step the server never sent", () => {
    render(
      <SubagentTrace
        {...props({ row: row({ objective: null, lastStep: null }) })}
      />,
    );

    const header = screen.getByRole("banner");
    expect(header.textContent).toContain("服务端未提供");
    // 不把 taskId 冒充成目标：标题本身只有「服务端未提供」。
    const heading = screen.getByRole("heading", { level: 2 });
    expect(heading.textContent).toBe("服务端未提供");
  });
});

describe("navigation", () => {
  it("offers the parent link only when the parent is a different task", () => {
    const parent = row({ taskId: "task-p", objective: "父任务", parentTaskId: null });
    const { rerender } = render(<SubagentTrace {...props({ parent })} />);
    expect(screen.getByRole("button", { name: "返回父任务 父任务" })).toBeTruthy();

    // 自引用不是父任务：不能给一个点下去回到原地的入口。
    rerender(<SubagentTrace {...props({ parent: row() })} />);
    expect(screen.queryByRole("button", { name: /返回父任务/ })).toBeNull();
  });

  it("drills into children through the same open operation", () => {
    const onDrill = vi.fn();
    const child = row({ taskId: "task-c", objective: "子任务一", parentTaskId: "task-a" });
    render(<SubagentTrace {...props({ children: [child], onDrill })} />);

    fireEvent.click(screen.getByRole("button", { name: "进入子任务 子任务一" }));
    expect(onDrill).toHaveBeenCalledWith("task-c");
  });

  it("never lists itself as its own child", () => {
    render(<SubagentTrace {...props({ children: [row()] })} />);
    expect(screen.queryByRole("button", { name: /进入子任务/ })).toBeNull();
  });

  it("collapses the children area with the keyboard", () => {
    const child = row({ taskId: "task-c", objective: "子任务一", parentTaskId: "task-a" });
    render(<SubagentTrace {...props({ children: [child] })} />);

    const toggle = screen.getByRole("button", { name: /^子任务 \d/ });
    expect(toggle.getAttribute("aria-expanded")).toBe("true");

    fireEvent.click(toggle);
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    expect(screen.queryByRole("button", { name: "进入子任务 子任务一" })).toBeNull();
  });

  it("goes back to the list", () => {
    const onBack = vi.fn();
    render(<SubagentTrace {...props({ onBack })} />);
    fireEvent.click(screen.getByRole("button", { name: "返回任务列表" }));
    expect(onBack).toHaveBeenCalled();
  });
});

describe("status", () => {
  it("says loading once, in the timeline", () => {
    render(
      <SubagentTrace {...props({ subscription: subscription({ phase: "loading", rows: [] }) })} />,
    );
    expect(screen.getAllByText(/正在加载轨迹/)).toHaveLength(1);
  });

  it("says empty once when a ready trace has no events", () => {
    render(<SubagentTrace {...props({ subscription: subscription({ rows: [] }) })} />);
    expect(screen.getByText("该任务暂无轨迹")).toBeTruthy();
  });

  it("keeps the history visible when the subscription fails", () => {
    render(
      <SubagentTrace
        {...props({ subscription: subscription({ phase: "error", error: "连接已断开" }) })}
      />,
    );

    expect(screen.getByText("first line")).toBeTruthy();
    expect(screen.getByText("连接已断开")).toBeTruthy();
    expect(screen.getByRole("button", { name: "重新订阅" })).toBeTruthy();
  });

  it("reports a disconnection without dropping what was read", () => {
    render(
      <SubagentTrace {...props({ subscription: subscription({ disconnected: true }) })} />,
    );
    expect(screen.getByText("first line")).toBeTruthy();
    expect(screen.getByText("连接已断开")).toBeTruthy();
  });

  it("offers a refresh once the trace is ready", () => {
    const onRetry = vi.fn();
    render(<SubagentTrace {...props({ onRetry })} />);
    fireEvent.click(screen.getByRole("button", { name: "刷新轨迹" }));
    expect(onRetry).toHaveBeenCalled();
  });

  it("says a paused task is paused instead of waiting forever", () => {
    render(
      <SubagentTrace
        {...props({ row: row({ state: "paused", finished: false, lastStep: null }) })}
      />,
    );
    expect(screen.getByText("已暂停")).toBeTruthy();
    expect(screen.queryByText(/等待新的执行进展/)).toBeNull();
  });

  it("waits only for a known active state", () => {
    render(<SubagentTrace {...props()} />);
    expect(screen.getByText("等待新的执行进展…")).toBeTruthy();
  });

  it("names the terminal state and stops waiting", () => {
    render(
      <SubagentTrace {...props({ row: row({ state: "failed", finished: true }) })} />,
    );
    expect(screen.getByText("失败")).toBeTruthy();
    expect(screen.queryByText("等待新的执行进展…")).toBeNull();
  });

  it("does not claim a task is running when the row is missing", () => {
    render(<SubagentTrace {...props({ row: null })} />);
    expect(screen.getByText("状态未知")).toBeTruthy();
    expect(screen.queryByText("等待新的执行进展…")).toBeNull();
    expect(screen.queryByText("运行中")).toBeNull();
  });
});

describe("task info", () => {
  it("opens the full information inside the timeline and focuses it", () => {
    render(<SubagentTrace {...props()} />);

    fireEvent.click(screen.getByRole("button", { name: "查看完整任务信息" }));
    const heading = screen.getByRole("heading", { name: "完整任务信息" });
    expect(document.activeElement).toBe(heading);
  });

  it("shows the real values and says so when the server sent none", () => {
    render(<SubagentTrace {...props({ row: row({ objective: null, lastStep: null }) })} />);

    fireEvent.click(screen.getByRole("button", { name: "查看完整任务信息" }));
    const region = screen.getByRole("heading", { name: "完整任务信息" }).parentElement!.parentElement!;
    expect(region.textContent).toContain("task-a");
    expect(region.textContent).toContain("服务端未提供");
  });
});

describe("composition", () => {
  it("scopes the draft change to the current task", () => {
    const onDraftChange = vi.fn();
    render(<SubagentTrace {...props({ onDraftChange })} />);

    fireEvent.change(screen.getByLabelText("向这个 agent 发送消息"), {
      target: { value: "next" },
    });
    expect(onDraftChange).toHaveBeenCalledWith("next");
  });

  it("stops through the composer using the same task", () => {
    const onPreview = vi.fn().mockResolvedValue(undefined);
    render(<SubagentTrace {...props({ onPreview })} />);

    fireEvent.click(screen.getByRole("button", { name: "停止任务" }));
    expect(onPreview).toHaveBeenCalled();
  });

  it("keeps the actions visible under a long trace", () => {
    const rows = Array.from({ length: 400 }, (_, i) =>
      traceRow(i + 1, "assistant_text", { text: `line ${i + 1}` }),
    );
    render(<SubagentTrace {...props({ subscription: subscription({ rows }) })} />);

    expect(screen.getByLabelText("向这个 agent 发送消息")).toBeTruthy();
    expect(screen.getByRole("button", { name: "停止任务" })).toBeTruthy();
  });
});

describe("isolation", () => {
  it("resets the children disclosure when the identity changes", () => {
    const child = row({ taskId: "task-c", objective: "子任务一", parentTaskId: "task-a" });
    const { rerender } = render(<SubagentTrace {...props({ children: [child] })} />);
    fireEvent.click(screen.getByRole("button", { name: /^子任务 \d/ }));

    rerender(
      <SubagentTrace
        {...props({
          identityKey: "b",
          taskId: "task-b",
          row: row({ taskId: "task-b" }),
          children: [row({ taskId: "task-d", objective: "另一子任务", parentTaskId: "task-b" })],
        })}
      />,
    );

    // 折叠状态属于上一条轨迹，不能带到下一条。
    expect(screen.getByRole("button", { name: /^子任务 \d/ }).getAttribute("aria-expanded")).toBe("true");
  });

  it("renders a raw payload instead of dropping an unparsable row", () => {
    const raw = traceRow(7, "future_kind", { a: 1 });
    render(<SubagentTrace {...props({ subscription: subscription({ rows: [raw] }) })} />);

    expect(screen.getByText("无法解析的事件")).toBeTruthy();
  });
});
