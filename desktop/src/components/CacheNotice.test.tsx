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
   * 的时间戳，格式化出 20758d。
   *
   * 但换成「更新时间未知」只修了一半：这句话仍然在**声称屏幕上是离线内容**。
   * `savedAt` 为 null 的真实成因恰恰相反——`cachedAt` 只在读到缓存条目时才
   * 有值，所以 null 等价于「这个会话没有内容缓存」。此时正文区是空的，提示条
   * 却在介绍一份不存在的缓存，同样是在说一件用户看不到的事。
   */
  it("does not claim offline content when nothing is cached", () => {
    render(<CacheNotice savedAt={null} />);
    const text = screen.getByRole("status").textContent ?? "";
    expect(text).not.toContain("离线内容");
    expect(text).not.toMatch(/\d{3,}d/);
    expect(text).not.toContain("20758");
  });

  it("treats a zero timestamp as not-cached instead of inventing an age", () => {
    render(<CacheNotice savedAt={0} />);
    const text = screen.getByRole("status").textContent ?? "";
    expect(text).not.toMatch(/\d{3,}d/);
    expect(text).not.toContain("离线内容");
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
