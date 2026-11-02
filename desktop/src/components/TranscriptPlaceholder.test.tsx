/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, cleanup } from "@testing-library/react";
import { TranscriptPlaceholder } from "./TranscriptPlaceholder";

afterEach(cleanup);

describe("TranscriptPlaceholder", () => {
  it("renders a skeleton with an accessible label while loading", () => {
    render(<TranscriptPlaceholder kind="loading" />);
    const el = screen.getByTestId("sync-skeleton");
    expect(el).toBeTruthy();
    // 骨架是纯装饰，必须对读屏用户隐藏，避免念一串无意义的空块。
    expect(el.getAttribute("aria-hidden")).toBe("true");
  });

  it("hides the decorative bars from assistive tech but keeps a live region", () => {
    render(<TranscriptPlaceholder kind="loading" />);
    // 状态本身要能被读屏念出来。
    expect(screen.getByRole("status").textContent).toContain("正在加载");
  });

  it("does not render a retry button while loading", () => {
    render(<TranscriptPlaceholder kind="loading" onRetry={() => {}} />);
    expect(screen.queryByRole("button")).toBeNull();
  });

  it("renders the failure reason and a retry button", () => {
    const onRetry = vi.fn();
    render(
      <TranscriptPlaceholder kind="failed" reason="内容加载超时，请重试" onRetry={onRetry} />,
    );
    expect(screen.getByTestId("sync-failed")).toBeTruthy();
    expect(screen.getByText("内容加载超时，请重试")).toBeTruthy();
    screen.getByRole("button", { name: "重试" }).click();
    expect(onRetry).toHaveBeenCalledTimes(1);
  });

  it("survives a missing reason", () => {
    render(<TranscriptPlaceholder kind="failed" onRetry={() => {}} />);
    expect(screen.getByTestId("sync-failed")).toBeTruthy();
    expect(screen.getByRole("button", { name: "重试" })).toBeTruthy();
  });

  it("does not render a retry button when no handler is given", () => {
    render(<TranscriptPlaceholder kind="failed" reason="x" />);
    expect(screen.queryByRole("button")).toBeNull();
  });

  it("renders the crash variant with its own wording", () => {
    render(<TranscriptPlaceholder kind="crashed" reason="render boom" />);
    expect(screen.getByTestId("sync-crashed")).toBeTruthy();
    expect(screen.getByText(/这段内容渲染失败/)).toBeTruthy();
    expect(screen.getByText("render boom")).toBeTruthy();
  });
});
