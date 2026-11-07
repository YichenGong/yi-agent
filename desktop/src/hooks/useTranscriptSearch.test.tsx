/** @vitest-environment jsdom */
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { render, cleanup, act } from "@testing-library/react";
import { useRef, useState } from "react";
import { useTranscriptSearch } from "./useTranscriptSearch";
import { createHeightBook } from "../lib/heightBook";
import type { Item } from "../lib/protocol";

let last: { total: number; active: number; api: any } = { total: 0, active: 0, api: null };
let jumps: number[] = [];

const msg = (id: string, text: string): Item => ({ type: "agentMessage", id, text }) as Item;

function Harness({
  items,
  query,
  structuralVersion = 0,
  open = true,
}: {
  items: Item[];
  query: string;
  structuralVersion?: number;
  open?: boolean;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const [book] = useState(() => createHeightBook(40));
  const state = useTranscriptSearch({
    containerRef: ref,
    open,
    query,
    items,
    structuralVersion,
    book,
    onJump: (i: number) => jumps.push(i),
  });
  last = { total: state.total, active: state.active, api: state };
  return <div ref={ref} data-testid="scroller" />;
}

beforeEach(() => {
  last = { total: 0, active: 0, api: null };
  jumps = [];
  // jsdom 不实现 scrollIntoView，不补会在 step 用例里抛。
  Element.prototype.scrollIntoView = vi.fn();
});
afterEach(cleanup);

describe("useTranscriptSearch (data layer)", () => {
  it("命中数覆盖全量历史，不只是挂载区", () => {
    // 窗口化后 DOM 里只有一页，按 mark 计数会严重少算。
    const items = Array.from({ length: 3000 }, (_, i) => msg(`m${i}`, "needle"));
    render(<Harness items={items} query="needle" />);
    expect(last.total).toBe(3000);
  });

  it("空查询计数为 0", () => {
    render(<Harness items={[msg("a", "x")]} query="  " />);
    expect(last.total).toBe(0);
  });

  it("关闭搜索时计数清零", () => {
    const items = [msg("a", "x")];
    const { rerender } = render(<Harness items={items} query="x" open />);
    expect(last.total).toBe(1);
    rerender(<Harness items={items} query="x" open={false} />);
    expect(last.total).toBe(0);
    expect(last.active).toBe(0);
  });

  it("next 回绕，按命中顺序推进", () => {
    const items = [msg("a", "x"), msg("b", "x"), msg("c", "x")];
    render(<Harness items={items} query="x" />);
    act(() => last.api.next());
    expect(last.active).toBe(1);
    act(() => last.api.next());
    act(() => last.api.next());
    expect(last.active).toBe(0);
  });

  it("prev 反向回绕", () => {
    const items = [msg("a", "x"), msg("b", "x")];
    render(<Harness items={items} query="x" />);
    act(() => last.api.prev());
    expect(last.active).toBe(1);
  });

  it("跳转未挂载的命中会把它的 item index 交给 onJump", () => {
    // 第 1200 条命中，窗口化后它绝不在首屏；必须靠 onJump 去开窗。
    const items = Array.from({ length: 3000 }, (_, i) =>
      msg(`m${i}`, i === 1200 ? "needle" : "hay"),
    );
    render(<Harness items={items} query="needle" />);
    expect(last.total).toBe(1);
    expect(jumps).toContain(1200);
  });

  it("同引用重渲染不重扫（流式文本 delta 不触发重扫）", () => {
    const items = [msg("a", "x"), msg("b", "y")];
    const { rerender } = render(<Harness items={items} query="x" structuralVersion={0} />);
    // 每次重扫都会把当前命中交给 onJump，所以 jumps 长度就是扫描次数的探针。
    const scansAfterFirstRender = jumps.length;
    expect(scansAfterFirstRender).toBeGreaterThan(0);
    // 流式期间：同一个 items 数组实例、原地改文本、结构版本不变。
    items[0] = msg("a", "x and more");
    rerender(<Harness items={items} query="x" structuralVersion={0} />);
    expect(jumps.length).toBe(scansAfterFirstRender);
  });

  it("结构性变更会重扫", () => {
    const items = [msg("a", "x")];
    const { rerender } = render(<Harness items={items} query="x" structuralVersion={0} />);
    expect(last.total).toBe(1);
    items.push(msg("b", "x"));
    rerender(<Harness items={items} query="x" structuralVersion={1} />);
    expect(last.total).toBe(2);
  });

  it("一条命中都没有时不调 onJump（不把用户甩到别处）", () => {
    render(<Harness items={[msg("a", "hay")]} query="needle" />);
    expect(last.total).toBe(0);
    expect(jumps).toEqual([]);
  });
});
