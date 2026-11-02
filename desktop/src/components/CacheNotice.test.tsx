/** @vitest-environment jsdom */
import { describe, it, expect, afterEach } from "vitest";
import { render, screen, cleanup } from "@testing-library/react";
import { CacheNotice } from "./CacheNotice";

afterEach(cleanup);

describe("CacheNotice", () => {
  it("says it is syncing while the catch-up is in flight", () => {
    render(<CacheNotice syncing />);
    expect(screen.getByRole("status").textContent).toBe("正在同步…");
  });

  it("names the failure instead of blaming the connection", () => {
    render(<CacheNotice failed />);
    const text = screen.getByRole("status").textContent ?? "";
    expect(text).toContain("内容加载失败");
    // 反向断言：失败态不许再说「断开」——连接可能明明是通的。
    expect(text).not.toContain("断开");
  });

  it("prefers the failure wording over the syncing wording", () => {
    render(<CacheNotice syncing failed />);
    expect(screen.getByRole("status").textContent).toContain("内容加载失败");
  });

  it("keeps the offline wording when nothing is cached", () => {
    render(<CacheNotice savedAt={null} />);
    expect(screen.getByRole("status").textContent).toBe("内容未缓存 · 连接断开");
  });

  it("shows an accessible motion-safe spinner only while syncing", () => {
    render(<CacheNotice syncing />);
    const indicator = screen.getByTestId("sync-indicator");
    expect(indicator.getAttribute("aria-hidden")).toBe("true");
    expect(indicator.className).toContain("size-3");
    expect(indicator.className).toContain("motion-safe:animate-spin");
    expect(indicator.className).not.toMatch(/(^|\s)animate-spin(\s|$)/);
    expect(screen.getByRole("status").textContent).toBe("正在同步…");
  });

  it("removes the spinner when syncing becomes failed or offline", () => {
    const { rerender } = render(<CacheNotice syncing />);
    expect(screen.getByTestId("sync-indicator")).toBeTruthy();
    rerender(<CacheNotice syncing failed />);
    expect(screen.queryByTestId("sync-indicator")).toBeNull();
    expect(screen.getByRole("status").textContent).toBe("内容加载失败 · 可重试");
    rerender(<CacheNotice savedAt={null} />);
    expect(screen.queryByTestId("sync-indicator")).toBeNull();
    expect(screen.getByRole("status").textContent).toBe("内容未缓存 · 连接断开");
  });

  it("does not render a spinner on initial failed or offline mounts", () => {
    const { rerender } = render(<CacheNotice failed />);
    expect(screen.queryByTestId("sync-indicator")).toBeNull();
    rerender(<CacheNotice savedAt={Date.now() - 60_000} />);
    expect(screen.queryByTestId("sync-indicator")).toBeNull();
    expect(screen.getByRole("status").textContent).toContain("离线内容");
  });
});
