/** @vitest-environment jsdom */
import { useRef } from "react";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { useTraceScroll, type TraceScroll } from "./useTraceScroll";
import { STICK_BOTTOM_THRESHOLD_PX } from "../lib/useStickToBottom";

afterEach(cleanup);

/**
 * A scroll box with real geometry.
 *
 * jsdom reports every rect as zero, which would make an anchor test pass for the
 * wrong reason (every offset is 0). This model gives each item a top in content
 * coordinates and shifts it by `scrollTop`, exactly like a browser does, so an
 * assertion about "the anchored row kept its offset" is meaningful.
 */
interface Box {
  scrollHeight: number;
  clientHeight: number;
  scrollTop: number;
  items: Map<string, { top: number; height: number }>;
}

let box: Box;
let restoreGeometry: (() => void) | null = null;

function installGeometry(): void {
  const original = Element.prototype.getBoundingClientRect;
  Element.prototype.getBoundingClientRect = function (this: Element) {
    const key = (this as HTMLElement).dataset?.traceKey;
    if (key !== undefined) {
      const item = box.items.get(key);
      if (item) {
        const top = item.top - box.scrollTop;
        return {
          top, bottom: top + item.height, height: item.height,
          left: 0, right: 100, width: 100, x: 0, y: top,
          toJSON: () => ({}),
        } as DOMRect;
      }
    }
    if ((this as HTMLElement).dataset?.traceViewport !== undefined) {
      return {
        top: 0, bottom: box.clientHeight, height: box.clientHeight,
        left: 0, right: 100, width: 100, x: 0, y: 0,
        toJSON: () => ({}),
      } as DOMRect;
    }
    return original.call(this);
  };
  restoreGeometry = () => {
    Element.prototype.getBoundingClientRect = original;
  };
}

function defineBox(node: HTMLElement): void {
  Object.defineProperty(node, "scrollHeight", { configurable: true, get: () => box.scrollHeight });
  Object.defineProperty(node, "clientHeight", { configurable: true, get: () => box.clientHeight });
  Object.defineProperty(node, "scrollTop", {
    configurable: true,
    get: () => box.scrollTop,
    set: (value: number) => {
      const max = Math.max(0, box.scrollHeight - box.clientHeight);
      box.scrollTop = Math.min(Math.max(0, value), max);
    },
  });
}

let latest: TraceScroll | null = null;

function Harness({ identityKey = "a", ready = false }: { identityKey?: string; ready?: boolean }) {
  const scroll = useTraceScroll(identityKey, ready, () => {});
  latest = scroll;
  const viewport = useRef<HTMLDivElement | null>(null);

  return (
    <div>
      <div
        ref={(node) => {
          scroll.ref.current = node;
          viewport.current = node;
          if (node) defineBox(node);
        }}
        data-trace-viewport=""
        data-following={String(scroll.following)}
        role="log"
      >
        <div ref={scroll.contentRef}>
          {[...box.items.keys()].map((key) => (
            <div key={key} data-trace-key={key}>
              {key}
            </div>
          ))}
          <div ref={scroll.taskInfoRef} hidden>
            <h3 ref={scroll.taskInfoHeadingRef} tabIndex={-1}>
              完整任务信息
            </h3>
          </div>
        </div>
      </div>
      <button
        type="button"
        onClick={() =>
          scroll.preserve(() => {
            // Loading 300px of earlier blocks: everything already on screen moves
            // down inside the content box, which is exactly what must be compensated.
            box.scrollHeight += 300;
            for (const item of box.items.values()) item.top += 300;
          })
        }
      >
        grow with preserve
      </button>
      <button type="button" onClick={() => { box.scrollHeight += 300; }}>grow</button>
      <button type="button" onClick={() => scroll.scrollToBottom()}>bottom</button>
      <button type="button" id="entry" onClick={(event) => scroll.showTaskInfo(() => {}, event.currentTarget)}>
        show info
      </button>
      <button type="button" onClick={() => scroll.restoreTaskInfo(() => {})}>hide info</button>
      <button
        type="button"
        onClick={() =>
          scroll.restoreTaskInfo(() => {
            // 收起信息区：内容整体上移 300px，正是必须补偿的那一步。
            box.scrollHeight -= 300;
            for (const item of box.items.values()) item.top -= 300;
          })
        }
      >
        hide info collapsing
      </button>
    </div>
  );
}

function scroller(): HTMLElement {
  const node = screen.getByRole("log");
  defineBox(node);
  return node;
}

function following(): string | undefined {
  return latest?.following === undefined ? undefined : String(latest.following);
}

beforeEach(() => {
  latest = null;
  box = {
    scrollHeight: 1000,
    clientHeight: 200,
    scrollTop: 500,
    items: new Map([
      ["a", { top: 400, height: 100 }],
      ["b", { top: 600, height: 100 }],
      ["c", { top: 900, height: 100 }],
    ]),
  };
  installGeometry();
});

afterEach(() => {
  restoreGeometry?.();
  restoreGeometry = null;
  vi.restoreAllMocks();
});

describe("following", () => {
  it("exposes the follow intent as state a reader can re-render on", () => {
    render(<Harness />);
    expect(following()).toBe("false");

    act(() => {
      box.scrollTop = 800;
      fireEvent.scroll(scroller());
    });
    expect(following()).toBe("true");
  });

  it("stops following when the user scrolls up", () => {
    render(<Harness />);
    act(() => {
      box.scrollTop = 800;
      fireEvent.scroll(scroller());
    });
    act(() => {
      box.scrollTop = 500;
      fireEvent.scroll(scroller());
    });

    expect(following()).toBe("false");
  });

  it("keeps the user's place when content grows while paused", () => {
    render(<Harness />);
    const before = box.scrollTop;
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "grow" }));
    });

    expect(box.scrollTop).toBe(before);
  });

  it("sticks to the bottom on growth while following", () => {
    render(<Harness />);
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "bottom" }));
    });
    act(() => {
      // The detail layer calls followIfAtBottom from a layout effect after the
      // blocks changed; here the growth and the call stand in for that pair.
      latest!.followIfAtBottom();
    });

    expect(box.scrollHeight - box.clientHeight - box.scrollTop).toBeLessThanOrEqual(
      STICK_BOTTOM_THRESHOLD_PX,
    );
  });
});

describe("preserve", () => {
  it("restores the anchored item's offset after the content changes", () => {
    render(<Harness />);
    // "b" sits 100px below the viewport top (600 - 500).
    expect(box.items.get("b")!.top - box.scrollTop).toBe(100);

    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "grow with preserve" }));
    });

    expect(box.items.get("b")!.top - box.scrollTop).toBe(100);
    expect(box.scrollTop).toBe(800);
  });

  it("releases follow so the next append does not pull the view down", () => {
    render(<Harness />);
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "bottom" }));
    });
    expect(following()).toBe("true");

    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "grow with preserve" }));
    });
    const afterPreserve = box.scrollTop;

    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "grow" }));
    });

    expect(box.scrollTop).toBe(afterPreserve);
    expect(following()).toBe("false");
  });

  it("keeps the pause through the asynchronous scroll event a restore generates", () => {
    render(<Harness />);
    // Start from "following": that is the state in which the browser's delayed
    // scroll event could silently re-enable follow after a restore.
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "bottom" }));
    });
    expect(following()).toBe("true");

    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "grow with preserve" }));
    });

    // Browsers dispatch scroll after we assign scrollTop. Without the programmatic
    // guard this reads as the user returning to the bottom and re-enables follow.
    // 跑到真正的底部（scrollHeight 已增长到 1300，max = 1100），否则共享 hook
    // 自己也不会认为贴底，守卫的缺失就被掩盖了。
    act(() => {
      box.scrollTop = box.scrollHeight;
      fireEvent.scroll(scroller());
    });

    expect(following()).toBe("false");
  });

  it("resumes following after a real scroll intent", () => {
    render(<Harness />);
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "bottom" }));
    });
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "grow with preserve" }));
    });

    act(() => {
      fireEvent.wheel(scroller());
      box.scrollTop = box.scrollHeight;
      fireEvent.scroll(scroller());
    });

    expect(following()).toBe("true");
  });
});

describe("task info round trip", () => {
  it("focuses the info heading after revealing it", () => {
    render(<Harness />);
    const focus = vi.spyOn(HTMLElement.prototype, "focus");
    const entry = screen.getByRole("button", { name: "show info" }) as HTMLButtonElement;

    act(() => {
      fireEvent.click(entry);
    });

    // Focusing with preventScroll keeps the outer conversation from jumping.
    expect(focus).toHaveBeenCalled();
    const heading = screen.getByText("完整任务信息");
    expect(focus.mock.instances).toContain(heading);
    expect(focus.mock.calls[0]?.[0]).toEqual({ preventScroll: true });
  });

  it("returns focus to the entry when the info closes", () => {
    render(<Harness />);
    const entry = screen.getByRole("button", { name: "show info" });
    act(() => {
      fireEvent.click(entry);
    });

    const focus = vi.spyOn(HTMLElement.prototype, "focus");
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "hide info" }));
    });

    expect(focus.mock.instances).toContain(entry);
    expect(focus.mock.calls[0]?.[0]).toEqual({ preventScroll: true });
  });

  it("restores the anchored offset after closing the info region", () => {
    render(<Harness />);
    // "b" sits 100px below the viewport top before the panel opens.
    expect(box.items.get("b")!.top - box.scrollTop).toBe(100);

    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "show info" }));
    });
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "hide info collapsing" }));
    });

    // 收起把人送回原处：那一段仍然停在同一个高度。
    expect(box.items.get("b")!.top - box.scrollTop).toBe(100);
  });

  it("falls back when the recorded entry is gone", () => {
    const fallback = vi.fn();
    function Host() {
      const scroll = useTraceScroll("a", true, fallback);
      return (
        <div>
          <div ref={scroll.ref} role="log">
            <div ref={scroll.contentRef}>
              <div ref={scroll.taskInfoRef}>
                <h3 ref={scroll.taskInfoHeadingRef} tabIndex={-1}>完整任务信息</h3>
              </div>
            </div>
          </div>
          <button type="button" onClick={(event) => scroll.showTaskInfo(() => {}, event.currentTarget)}>
            show info
          </button>
          <button type="button" onClick={() => scroll.restoreTaskInfo(() => {})}>hide info</button>
      <button
        type="button"
        onClick={() =>
          scroll.restoreTaskInfo(() => {
            // 收起信息区：内容整体上移 300px，正是必须补偿的那一步。
            box.scrollHeight -= 300;
            for (const item of box.items.values()) item.top -= 300;
          })
        }
      >
        hide info collapsing
      </button>
        </div>
      );
    }
    render(<Host />);
    const entry = screen.getByRole("button", { name: "show info" });
    act(() => {
      fireEvent.click(entry);
    });
    // The rail row that opened the panel is removed while the info is open.
    entry.remove();

    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "hide info" }));
    });

    expect(fallback).toHaveBeenCalled();
  });
});

describe("cleanup", () => {
  it("removes its listeners and observer on unmount", () => {
    const remove = vi.spyOn(HTMLElement.prototype, "removeEventListener");
    const { unmount } = render(<Harness />);
    unmount();

    expect(remove.mock.calls.some(([type]) => type === "scroll")).toBe(true);
  });

  it("re-initialises when the identity changes", () => {
    const { rerender } = render(<Harness identityKey="a" />);
    act(() => {
      box.scrollTop = 800;
      fireEvent.scroll(scroller());
    });
    expect(following()).toBe("true");

    rerender(<Harness identityKey="b" />);

    expect(latest!.identityKey).toBe("b");
  });
});
