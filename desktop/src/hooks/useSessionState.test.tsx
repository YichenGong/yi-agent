/** @vitest-environment jsdom */
import { describe, it, expect, vi } from "vitest";
import { renderHook, act } from "@testing-library/react";
import { useSessionState } from "./useSessionState";
import { ThreadStore } from "../lib/threadStore";
import { SubagentRailStore } from "../lib/subagents";

// 宿主层的像素断言依赖真实布局，jsdom 下不可靠；位置的**真实**行为已由
// `ChatView` 的测试钉住。这里只钉存储语义：按会话分开、`undefined`（从没记过）
// 与 `null`（记过且当时贴底）不是一回事、删除会话即丢弃。
const mount = () =>
  renderHook(() =>
    useSessionState({
      rpc: () => null,
      store: new ThreadStore(),
      force: vi.fn(),
      setCurrentError: vi.fn(),
      railStore: new SubagentRailStore(),
      initialListCache: null,
    }),
  );

describe("useSessionState view anchors", () => {
  it("distinguishes never-recorded from recorded-at-bottom", () => {
    const { result } = mount();
    expect(result.current.anchorFor("t1")).toBeUndefined();
    act(() => result.current.rememberView("t1", null));
    expect(result.current.anchorFor("t1")).toBeNull();
  });

  it("keeps each thread's anchor separate", () => {
    const { result } = mount();
    act(() => {
      result.current.rememberView("t1", { itemId: "a", offset: -100 });
      result.current.rememberView("t2", { itemId: "b", offset: 0 });
    });
    expect(result.current.anchorFor("t1")).toEqual({ itemId: "a", offset: -100 });
    expect(result.current.anchorFor("t2")).toEqual({ itemId: "b", offset: 0 });
  });

  it("overwrites the anchor on every report", () => {
    const { result } = mount();
    act(() => result.current.rememberView("t1", { itemId: "a", offset: -100 }));
    act(() => result.current.rememberView("t1", { itemId: "z", offset: -20 }));
    expect(result.current.anchorFor("t1")).toEqual({ itemId: "z", offset: -20 });
  });

  it("forgets a deleted thread's anchor", () => {
    const { result } = mount();
    act(() => result.current.rememberView("t1", { itemId: "a", offset: 0 }));
    act(() => result.current.forgetView("t1"));
    expect(result.current.anchorFor("t1")).toBeUndefined();
  });
});
