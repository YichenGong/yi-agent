/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, fireEvent, cleanup } from "@testing-library/react";
import type { ComponentProps } from "react";
import { SearchBar } from "./SearchBar";

afterEach(cleanup);

function setup(over: Partial<ComponentProps<typeof SearchBar>> = {}) {
  const props = {
    value: "",
    total: 0,
    active: 0,
    onChange: vi.fn(),
    onNext: vi.fn(),
    onPrev: vi.fn(),
    onClose: vi.fn(),
    ...over,
  };
  render(<SearchBar {...props} />);
  return props;
}

describe("SearchBar", () => {
  it("autofocuses the input", () => {
    setup();
    expect(document.activeElement).toBe(screen.getByRole("searchbox"));
  });

  it("reports typing", () => {
    const props = setup();
    fireEvent.change(screen.getByRole("searchbox"), { target: { value: "abc" } });
    expect(props.onChange).toHaveBeenCalledWith("abc");
  });

  it("shows a 1-based position over the total", () => {
    setup({ value: "x", total: 17, active: 2 });
    expect(screen.getByText("3/17")).toBeTruthy();
  });

  it("says there are no results", () => {
    setup({ value: "zzz", total: 0 });
    expect(screen.getByText("无结果")).toBeTruthy();
  });

  it("calls onNext on Enter and onPrev on Shift+Enter", () => {
    const props = setup({ value: "x", total: 3 });
    const box = screen.getByRole("searchbox");
    fireEvent.keyDown(box, { key: "Enter" });
    expect(props.onNext).toHaveBeenCalled();
    fireEvent.keyDown(box, { key: "Enter", shiftKey: true });
    expect(props.onPrev).toHaveBeenCalled();
  });

  it("does not treat an IME-confirmed Enter as navigation", () => {
    // 中文输入法用回车确认候选词时不该跳下一个命中。
    const props = setup({ value: "搜索", total: 3 });
    fireEvent.keyDown(screen.getByRole("searchbox"), { key: "Enter", keyCode: 229 });
    expect(props.onNext).not.toHaveBeenCalled();
  });

  it("wires the next / previous / close buttons", () => {
    const props = setup({ value: "x", total: 3 });
    fireEvent.click(screen.getByRole("button", { name: "下一个" }));
    expect(props.onNext).toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "上一个" }));
    expect(props.onPrev).toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "关闭查找" }));
    expect(props.onClose).toHaveBeenCalled();
  });

  it("disables navigation when there is nothing to move through", () => {
    setup({ value: "zzz", total: 0 });
    expect((screen.getByRole("button", { name: "下一个" }) as HTMLButtonElement).disabled).toBe(
      true,
    );
  });
});
