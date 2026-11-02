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
});
