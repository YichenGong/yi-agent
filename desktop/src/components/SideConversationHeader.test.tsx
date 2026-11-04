/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, fireEvent } from "@testing-library/react";
import type { SideSummaryWire } from "../lib/generated/SideSummaryWire";
import { SideConversationHeader } from "./SideConversationHeader";

afterEach(cleanup);

function summary(over: Partial<SideSummaryWire> = {}): SideSummaryWire {
  return {
    side_id: "s1",
    parent_thread_id: "t1",
    status: "idle",
    snapshot_revision: 1,
    last_seq: 0,
    active_turn_id: null,
    expires_at: null,
    generation: 0,
    stopping: false,
    ...over,
  };
}

function props(over: Partial<Parameters<typeof SideConversationHeader>[0]> = {}) {
  return {
    summary: summary(),
    parentStatus: "空闲",
    canControl: true,
    onBack: vi.fn(),
    onClose: vi.fn(),
    onStop: vi.fn(),
    ...over,
  };
}

describe("SideConversationHeader", () => {
  it("says it is a side conversation built on the creation-time context", () => {
    render(<SideConversationHeader {...props()} />);
    expect(screen.getByText("旁问")).toBeTruthy();
    expect(screen.getByText(/基于创建时上下文/)).toBeTruthy();
    expect(screen.getByText("只读")).toBeTruthy();
  });

  it("routes the parent status to back, never to a side action", () => {
    const p = props({ parentStatus: "等待审批" });
    render(<SideConversationHeader {...p} />);
    fireEvent.click(screen.getByRole("button", { name: /父：等待审批/ }));
    // 父在等审批时点它是「回去处理」，不是「让旁支处理」。
    expect(p.onBack).toHaveBeenCalledTimes(1);
    expect(p.onClose).not.toHaveBeenCalled();
    expect(p.onStop).not.toHaveBeenCalled();
  });

  it("always offers back", () => {
    const p = props({ canControl: false });
    render(<SideConversationHeader {...p} />);
    fireEvent.click(screen.getByRole("button", { name: "返回主会话" }));
    expect(p.onBack).toHaveBeenCalledTimes(1);
  });

  it("offers close only when the device can control", () => {
    const { unmount } = render(<SideConversationHeader {...props()} />);
    expect(screen.getByRole("button", { name: "关闭旁问" })).toBeTruthy();
    unmount();

    render(<SideConversationHeader {...props({ canControl: false })} />);
    // Observe：不渲染而不是渲染成禁用——一个永远点不动的按钮只会让人反复试。
    expect(screen.queryByRole("button", { name: "关闭旁问" })).toBeNull();
  });

  it("hides close while the server is already stopping", () => {
    render(<SideConversationHeader {...props({ summary: summary({ stopping: true }) })} />);
    expect(screen.queryByRole("button", { name: "关闭旁问" })).toBeNull();
  });

  it("disables close while a close request is in flight", () => {
    render(<SideConversationHeader {...props({ closing: true })} />);
    const button = screen.getByRole("button", {
      name: "关闭旁问",
    }) as HTMLButtonElement;
    expect(button.disabled).toBe(true);
  });

  it("offers Stop only while the side itself is running", () => {
    const { unmount } = render(<SideConversationHeader {...props()} />);
    expect(screen.queryByRole("button", { name: "停止旁问" })).toBeNull();
    unmount();

    const p = props({ summary: summary({ status: "running" }) });
    render(<SideConversationHeader {...p} />);
    fireEvent.click(screen.getByRole("button", { name: "停止旁问" }));
    expect(p.onStop).toHaveBeenCalledTimes(1);
    // Stop 不停父、也不等于返回。
    expect(p.onBack).not.toHaveBeenCalled();
  });

  it("reports each lifecycle status in words", () => {
    const cases: Array<[SideSummaryWire["status"], string]> = [
      ["creating", "正在建立"],
      ["idle", "空闲"],
      ["running", "正在回答"],
      ["failed", "已失败"],
      ["closing", "正在关闭"],
      ["closed", "已关闭"],
    ];
    for (const [status, text] of cases) {
      const { unmount } = render(
        <SideConversationHeader {...props({ summary: summary({ status }) })} />,
      );
      expect(screen.getByText(text)).toBeTruthy();
      unmount();
    }
  });

  it("says how long the side will live, and admits when it already expired", () => {
    const soon = Date.now() + 90_000;
    const { unmount } = render(
      <SideConversationHeader {...props({ summary: summary({ expires_at: soon }) })} />,
    );
    expect(screen.getByText(/2 分钟后自动清理/)).toBeTruthy();
    unmount();

    render(
      <SideConversationHeader
        {...props({ summary: summary({ expires_at: Date.now() - 1000 }) })}
      />,
    );
    // 已过期就照实说：假装还在会让人白等一条永远不来的回答。
    expect(screen.getByText("已过期")).toBeTruthy();
  });
});
