/** @vitest-environment jsdom */
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { render, cleanup, screen, act } from "@testing-library/react";
import { useRef } from "react";
import { useTranscriptSearch } from "./useTranscriptSearch";

// 捕获最近一次渲染的返回值。测试只关心钩子对外暴露的三个数。
let last: { total: number; active: number; api: any } = { total: 0, active: 0, api: null };

function Harness({ open, query, marks }: { open: boolean; query: string; marks: number }) {
  const ref = useRef<HTMLDivElement>(null);
  const state = useTranscriptSearch({ containerRef: ref, open, query });
  last = { total: state.total, active: state.active, api: state };
  return (
    <div ref={ref} data-testid="scroller">
      {Array.from({ length: marks }, (_, i) => (
        <mark key={i} className="search-hit" data-testid={`hit-${i}`}>
          x
        </mark>
      ))}
    </div>
  );
}

beforeEach(() => {
  last = { total: 0, active: 0, api: null };
  // jsdom 不实现 scrollIntoView，不补就会在 step 用例里抛。
  Element.prototype.scrollIntoView = vi.fn();
});
afterEach(cleanup);

describe("useTranscriptSearch", () => {
  it("counts the marks in the container", () => {
    render(<Harness open query="x" marks={3} />);
    expect(last.total).toBe(3);
  });

  it("does not count when closed", () => {
    render(<Harness open={false} query="x" marks={3} />);
    expect(last.total).toBe(0);
    expect(last.active).toBe(0);
  });

  it("marks the active hit in the DOM", () => {
    render(<Harness open query="x" marks={3} />);
    expect(screen.getByTestId("hit-0").getAttribute("data-search-active")).toBe("true");
    expect(screen.getByTestId("hit-1").getAttribute("data-search-active")).toBe(null);
  });

  it("clamps the active index when the hit count shrinks", () => {
    const { rerender } = render(<Harness open query="x" marks={3} />);
    rerender(<Harness open query="xx" marks={1} />);
    expect(last.total).toBe(1);
    expect(last.active).toBe(0);
  });

  it("removes the active attribute once closed", () => {
    const { rerender } = render(<Harness open query="x" marks={2} />);
    rerender(<Harness open={false} query="x" marks={2} />);
    expect(screen.getByTestId("hit-0").getAttribute("data-search-active")).toBe(null);
  });

  it("steps and wraps through hits", () => {
    render(<Harness open query="x" marks={2} />);
    // 必须包 act：next() 同步 setState，不包则断言前拿到的还是上一次渲染的值。
    act(() => last.api.next());
    expect(last.active).toBe(1);
    act(() => last.api.next());
    expect(last.active).toBe(0);
    act(() => last.api.prev());
    expect(last.active).toBe(1);
  });

  it("scrolls to the active hit when stepping", () => {
    render(<Harness open query="x" marks={2} />);
    act(() => last.api.next());
    expect(Element.prototype.scrollIntoView).toHaveBeenCalled();
  });
});
