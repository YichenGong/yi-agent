/** @vitest-environment jsdom */
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { CacheNotice } from "./CacheNotice";

afterEach(cleanup);

describe("CacheNotice", () => {
  it("labels the content as offline and shows when it was cached", () => {
    render(<CacheNotice savedAt={Date.now() - 3 * 60_000} />);
    expect(screen.getByRole("status").textContent).toContain("离线内容");
    expect(screen.getByRole("status").textContent).toContain("3m");
  });

  /**
   * 用户报的「20xxxd」：`savedAt` 未知时旧代码 `savedAt ?? 0` 把 0 当 1970 年
   * 的时间戳，格式化出 20758d。手机冷启动握手尚未完成（`offline` 为真）而
   * `thread/resume` 已在途时就会走到这条路径——那个会话可能**根本没有**内容
   * 缓存，`cachedAt` 里没有它，于是这里收到 null。
   */
  it("says the age is unknown instead of rendering thousands of days", () => {
    render(<CacheNotice savedAt={null} />);
    const text = screen.getByRole("status").textContent ?? "";
    expect(text).toContain("离线内容");
    expect(text).not.toMatch(/\d{3,}d/);
    expect(text).toContain("未知");
  });

  it("treats a zero timestamp as unknown too", () => {
    render(<CacheNotice savedAt={0} />);
    expect(screen.getByRole("status").textContent ?? "").not.toMatch(/\d{3,}d/);
  });
});

describe("CacheNotice syncing", () => {
  it("says it is syncing instead of showing an offline age", () => {
    render(<CacheNotice syncing />);
    const text = screen.getByRole("status").textContent ?? "";
    expect(text).toContain("正在同步");
    expect(text).not.toContain("离线内容");
  });
});
