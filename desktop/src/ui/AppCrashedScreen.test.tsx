/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, cleanup } from "@testing-library/react";
import { AppCrashedScreen } from "./AppCrashedScreen";

afterEach(cleanup);

describe("AppCrashedScreen", () => {
  it("explains the state and offers a reload button", () => {
    render(<AppCrashedScreen />);
    expect(screen.getByText(/界面出了点问题/)).toBeTruthy();
    expect(screen.getByRole("button", { name: "重新加载" })).toBeTruthy();
  });

  it("reloads the page when the button is pressed", () => {
    // 组件接受可注入的 onReload，正是为了在 jsdom 下断言到行为
    // （location.reload 在 jsdom 里不可 spy）。
    const onReload = vi.fn();
    render(<AppCrashedScreen onReload={onReload} />);
    screen.getByRole("button", { name: "重新加载" }).click();
    expect(onReload).toHaveBeenCalledTimes(1);
  });
});
