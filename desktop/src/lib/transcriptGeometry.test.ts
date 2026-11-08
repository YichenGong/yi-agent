/** @vitest-environment jsdom */
import { afterEach, describe, expect, it } from "vitest";
import { measureTranscript } from "./transcriptGeometry";

const SCROLLER_TOP = 100;
const SCROLL_TOP = 5000;
const VIEWPORT = 800;
// 内容坐标下的视口： [5000, 5800)

function rect(top: number, bottom: number): DOMRect {
  return {
    top,
    bottom,
    left: 0,
    right: 0,
    width: 0,
    height: bottom - top,
    x: 0,
    y: top,
    toJSON: () => ({}),
  } as DOMRect;
}

type Row = { id: string; index: number; top: number; bottom: number; region: "history" | "tail" };

/** 每行给出**内容坐标**，helper 转成 getBoundingClientRect 的视口坐标。 */
function mount(rows: Row[]): HTMLElement {
  const scroller = document.createElement("div");
  scroller.scrollTop = SCROLL_TOP;
  Object.defineProperty(scroller, "clientHeight", { value: VIEWPORT, configurable: true });
  scroller.getBoundingClientRect = () => rect(SCROLLER_TOP, SCROLLER_TOP + VIEWPORT);
  for (const r of rows) {
    const el = document.createElement("div");
    el.dataset.itemId = r.id;
    el.dataset.itemIndex = String(r.index);
    el.dataset.transcriptRegion = r.region;
    el.getBoundingClientRect = () =>
      rect(r.top - SCROLL_TOP + SCROLLER_TOP, r.bottom - SCROLL_TOP + SCROLLER_TOP);
    scroller.appendChild(el);
  }
  document.body.appendChild(scroller);
  return scroller;
}

afterEach(() => {
  document.body.innerHTML = "";
});

describe("measureTranscript", () => {
  it("未布局或没有行时返回 null，让调用方暂停而不是重置跟随意图", () => {
    expect(measureTranscript(mount([]))).toBeNull();
    expect(measureTranscript(document.createElement("div"))).toBeNull();
  });

  it("visible 用 data-item-index 的绝对序号，不是挂载区相对序号", () => {
    const el = mount([
      { id: "a", index: 340, top: 4700, bottom: 4900, region: "history" },
      { id: "b", index: 341, top: 5000, bottom: 5100, region: "history" },
      { id: "c", index: 342, top: 5700, bottom: 5800, region: "history" },
    ]);
    expect(measureTranscript(el)?.visible).toEqual({ first: 341, last: 342 });
  });

  it("历史 gap 只由历史行算，不被离得很远的 tail 掩盖", () => {
    // 历史区底边在视口底之上 2300px（还有历史没挂载），tail 单独挂在视口内。
    // 旧实现取「首末挂载行」= 历史首 + tail 末，于是 gapBottom=600 看起来
    // 「已覆盖视口」，不再向下开窗 —— 这正是停住不动、只能靠切回触发的形状。
    const el = mount([
      { id: "h1", index: 340, top: 3000, bottom: 3100, region: "history" },
      { id: "h2", index: 599, top: 3400, bottom: 3500, region: "history" },
      { id: "tail", index: 999, top: 5100, bottom: 5200, region: "tail" },
    ]);
    const m = measureTranscript(el);
    expect(m?.history).toEqual({
      first: 340,
      last: 599,
      gapTopPx: 3000 - SCROLL_TOP,
      gapBottomPx: SCROLL_TOP + VIEWPORT - 3500,
    });
    expect(m?.history?.gapBottomPx).toBe(2300);
    expect(m?.tailVisible).toBe(true);
    expect(m?.visible).toEqual({ first: 999, last: 999 });
    expect(m?.viewportTop).toBe(SCROLL_TOP);
    expect(m?.viewportBottom).toBe(SCROLL_TOP + VIEWPORT);
  });

  it("只有 tail 在视口内时 history 为 null，tailVisible 为真", () => {
    const el = mount([{ id: "tail", index: 999, top: 5400, bottom: 5600, region: "tail" }]);
    const m = measureTranscript(el);
    expect(m?.history).toBeNull();
    expect(m?.tailVisible).toBe(true);
    expect(m?.visible).toEqual({ first: 999, last: 999 });
  });

  it("上下 spacer 内没有相交行时 visible 为 null，但历史边界仍可读", () => {
    const el = mount([
      { id: "h1", index: 340, top: 2000, bottom: 2100, region: "history" },
      { id: "h2", index: 599, top: 2400, bottom: 2500, region: "history" },
    ]);
    const m = measureTranscript(el);
    expect(m?.visible).toBeNull();
    expect(m?.tailVisible).toBe(false);
    expect(m?.history?.gapBottomPx).toBe(SCROLL_TOP + VIEWPORT - 2500);
  });
});
