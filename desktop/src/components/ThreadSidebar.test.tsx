/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, fireEvent, cleanup, screen, act } from "@testing-library/react";
import type { ComponentProps } from "react";
import { ThreadSidebar } from "./ThreadSidebar";
import { LONG_PRESS_MS } from "../lib/useLongPress";
import { DEFAULT_TAG_NAMES, resolveTagNames } from "../lib/threadTags";
import type { ThreadSummary, Workspace, WorkspaceGroup } from "../lib/protocol";
import {
  DEFAULT_SIDEBAR_WIDTH,
  MIN_SIDEBAR_WIDTH,
  MAX_SIDEBAR_WIDTH,
  SIDEBAR_WIDTH_STORAGE_KEY,
} from "../lib/sidebarWidth";

afterEach(() => {
  cleanup();
  localStorage.clear();
});

function thread(id: string, title: string, cwd: string, tag?: string): ThreadSummary {
  return { thread_id: id, cwd, model: "m", created_at: 0, updated_at: 0, title, tag };
}

function pinnedThread(id: string, title: string): ThreadSummary {
  return { thread_id: id, cwd: "/work/projA", model: "m", created_at: 0, updated_at: 0, title, pinned: true };
}

const groups: WorkspaceGroup[] = [
  {
    workspace: "/work/projA",
    exists: true,
    threads: [thread("1", "alpha-thread", "/work/projA")],
  },
  {
    workspace: "/work/projB",
    exists: false,
    threads: [thread("2", "beta-thread", "/work/projB")],
  },
];

function sidebarProps(overrides: Partial<ComponentProps<typeof ThreadSidebar>> = {}) {
  const props: ComponentProps<typeof ThreadSidebar> = {
    groups,
    workspaces: [],
    currentId: null,
    pinned: [],
    statuses: new Map(),
    unread: new Map(),
    onSelect: vi.fn(),
    onRename: vi.fn(),
    onDelete: vi.fn(),
    onTogglePin: vi.fn(),
    onReorderPinned: vi.fn(),
    onNew: vi.fn(),
    onRemoveWorkspace: vi.fn(),
    onBrowse: vi.fn(),
    boards: [],
    boardSummaries: {},
    selectedBoard: null,
    onCreateBoard: vi.fn(),
    onRemoveBoard: vi.fn(),
    onOpenBoard: vi.fn(),
    onOpenSettings: vi.fn(),
    tagNames: DEFAULT_TAG_NAMES,
    onSetTag: vi.fn(),
    ...overrides,
  };
  return props;
}

function renderSidebar(overrides: Partial<ComponentProps<typeof ThreadSidebar>> = {}) {
  return render(<ThreadSidebar {...sidebarProps(overrides)} />);
}

const newThreadTrigger = (container: HTMLElement) =>
  container.querySelector<HTMLButtonElement>('button[aria-haspopup="menu"]')!;

describe("ThreadSidebar", () => {
  it("keeps the rename field open when Enter confirms an IME candidate", () => {
    const onRename = vi.fn();
    const { container } = renderSidebar({ onRename });

    fireEvent.doubleClick(screen.getByText("alpha-thread"));
    const field = container.querySelector<HTMLInputElement>("[data-rename-input]")!;
    fireEvent.change(field, { target: { value: "中文名" } });

    // macOS WKWebView rebuilds the committing keydown after tearing down the
    // composition: isComposing is false by then, keyCode is forced to 229.
    fireEvent.keyDown(field, { key: "Enter", keyCode: 229 });
    expect(onRename).not.toHaveBeenCalled();
    expect(container.querySelector("[data-rename-input]")).not.toBeNull();

    fireEvent.keyDown(field, { key: "Enter" });
    expect(onRename).toHaveBeenCalledWith("1", "中文名");
    expect(container.querySelector("[data-rename-input]")).toBeNull();
  });

  it("renders each group's basename and its thread rows", () => {
    const { container } = renderSidebar();
    expect(container.textContent).toContain("projA");
    expect(container.textContent).toContain("projB");
    expect(container.textContent).toContain("alpha-thread");
    expect(container.textContent).toContain("beta-thread");
  });

  it("marks a missing workspace while keeping its full path in the title", () => {
    const { container } = renderSidebar();
    const missing = container.querySelector('[title="/work/projB"]');
    expect(missing).not.toBeNull();
    expect(missing!.textContent).toContain("(missing)");

    const present = container.querySelector('[title="/work/projA"]');
    expect(present).not.toBeNull();
    expect(present!.textContent).not.toContain("(missing)");
  });

  it("hides a group's thread rows when collapsed and restores them on expand", () => {
    const { container } = renderSidebar();
    fireEvent.click(container.querySelector('[aria-label="Collapse"]')!);
    expect(container.textContent).not.toContain("alpha-thread");
    expect(container.textContent).toContain("beta-thread");

    fireEvent.click(container.querySelector('[aria-label="Expand"]')!);
    expect(container.textContent).toContain("alpha-thread");
  });

  it("lists recent workspaces in the New-thread dropdown and disables missing ones", () => {
    const onNew = vi.fn();
    const workspaces: Workspace[] = [
      { path: "/a/alpha", exists: true },
      { path: "/b/beta", exists: false },
    ];
    const { container } = renderSidebar({ workspaces, onNew });

    fireEvent.click(newThreadTrigger(container));
    const menu = container.querySelector('[role="menu"]');
    expect(menu).not.toBeNull();
    expect(menu!.textContent).toContain("alpha");
    expect(menu!.textContent).toContain("beta");
    expect(menu!.textContent).toContain("Browse…");

    const items = Array.from(menu!.querySelectorAll<HTMLButtonElement>('[role="menuitem"]'));
    const alpha = items.find((el) => el.textContent === "alpha")!;
    const beta = items.find((el) => el.textContent === "beta")!;
    expect(alpha.disabled).toBe(false);
    expect(beta.disabled).toBe(true);

    fireEvent.click(alpha);
    expect(onNew).toHaveBeenCalledWith("/a/alpha");
  });

  it("calls onBrowse directly when there are no recent workspaces", () => {
    const onBrowse = vi.fn();
    const { container } = renderSidebar({ workspaces: [], onBrowse });
    fireEvent.click(newThreadTrigger(container));
    expect(onBrowse).toHaveBeenCalledTimes(1);
  });

  it("opens a group's context menu with the keyboard and closes it on Escape", () => {
    const { container } = renderSidebar();
    const header = container.querySelector<HTMLElement>("[data-ws-header]")!;
    expect(header.getAttribute("aria-expanded")).toBe("false");

    fireEvent.keyDown(header, { key: "Enter" });
    expect(header.getAttribute("aria-expanded")).toBe("true");
    const menu = container.querySelector('[role="menu"]');
    expect(menu).not.toBeNull();
    expect(menu!.textContent).toContain("New thread here");
    expect(menu!.textContent).toContain("Remove from list");

    fireEvent.keyDown(document, { key: "Escape" });
    expect(container.querySelector('[role="menu"]')).toBeNull();
    expect(header.getAttribute("aria-expanded")).toBe("false");
  });

  it("does not open the group menu when Enter/Space bubbles from the collapse caret", () => {
    const { container } = renderSidebar();
    const header = container.querySelector<HTMLElement>("[data-ws-header]")!;
    const caret = header.querySelector<HTMLButtonElement>('button[aria-label="Collapse"]')!;

    // jsdom does not perform Enter/Space button activation, so a keydown on the
    // caret reproduces exactly the bubble path the header handler must ignore.
    // Without the `e.target === e.currentTarget` guard the header would open the
    // group menu here — and in a real webview its preventDefault would also
    // swallow the caret's Enter activation, leaving no keyboard way to collapse.
    fireEvent.keyDown(caret, { key: "Enter" });
    expect(container.querySelector('[role="menu"]')).toBeNull();
    expect(header.getAttribute("aria-expanded")).toBe("false");

    fireEvent.keyDown(caret, { key: " " });
    expect(container.querySelector('[role="menu"]')).toBeNull();
    expect(header.getAttribute("aria-expanded")).toBe("false");

    // The caret's own activation (what a real Enter/Space would trigger) still
    // collapses the group.
    fireEvent.click(caret);
    expect(container.textContent).not.toContain("alpha-thread");
    expect(header.querySelector('button[aria-label="Expand"]')).not.toBeNull();
  });

  it("renders a colour dot for a tagged thread and nothing for an untagged one", () => {
    const tagged: WorkspaceGroup[] = [
      {
        workspace: "/work/projA",
        exists: true,
        threads: [thread("1", "alpha-thread", "/work/projA", "red")],
      },
      {
        workspace: "/work/projB",
        exists: true,
        threads: [thread("2", "beta-thread", "/work/projB")],
      },
    ];
    const { container } = renderSidebar({ groups: tagged, tagNames: resolveTagNames({ red: "紧急" }) });
    const dots = container.querySelectorAll("[data-thread-tag]");
    expect(dots.length).toBe(1);
    expect(dots[0].getAttribute("data-thread-tag")).toBe("red");
    expect(dots[0].getAttribute("aria-label")).toBe("紧急");
    // 无标签的行不渲染任何色点节点。
    const row2 = container.querySelector('[data-thread-row="2"]')!;
    expect(row2.querySelector("[data-thread-tag]")).toBeNull();
    // 标题仍然渲染（色点不能顶掉标题）。
    expect(screen.getByText("beta-thread")).toBeTruthy();
  });

  it("does not render a dot for an invalid tag id", () => {
    const dirty: WorkspaceGroup[] = [
      {
        workspace: "/work/projA",
        exists: true,
        threads: [thread("1", "alpha-thread", "/work/projA", "chartreuse")],
      },
    ];
    const { container } = renderSidebar({ groups: dirty });
    expect(container.querySelector("[data-thread-tag]")).toBeNull();
  });

  it("opens the thread menu on right-click and applies a tag", () => {
    const onSetTag = vi.fn();
    const { container } = renderSidebar({ onSetTag });
    const row = container.querySelector<HTMLElement>('[data-thread-row="1"]')!;
    fireEvent.contextMenu(row);
    const menu = container.querySelector("[data-thread-menu]");
    expect(menu).toBeTruthy();
    fireEvent.click(screen.getByRole("menuitemradio", { name: "红" }));
    expect(onSetTag).toHaveBeenCalledWith("1", "red");
  });

  it("clears the tag from the menu", () => {
    const onSetTag = vi.fn();
    const { container } = renderSidebar({ onSetTag });
    fireEvent.contextMenu(container.querySelector<HTMLElement>('[data-thread-row="1"]')!);
    fireEvent.click(screen.getByRole("menuitemradio", { name: "无标签" }));
    expect(onSetTag).toHaveBeenCalledWith("1", null);
  });

  it("marks the current tag as checked", () => {
    const tagged: WorkspaceGroup[] = [
      {
        workspace: "/work/projA",
        exists: true,
        threads: [thread("1", "alpha-thread", "/work/projA", "blue")],
      },
    ];
    const { container } = renderSidebar({ groups: tagged });
    fireEvent.contextMenu(container.querySelector<HTMLElement>('[data-thread-row="1"]')!);
    expect(screen.getByRole("menuitemradio", { name: "蓝" }).getAttribute("aria-checked")).toBe("true");
    expect(screen.getByRole("menuitemradio", { name: "红" }).getAttribute("aria-checked")).toBe("false");
    expect(screen.getByRole("menuitemradio", { name: "无标签" }).getAttribute("aria-checked")).toBe("false");
  });

  it("still opens the same menu on long-press in mobile", async () => {
    vi.useFakeTimers();
    try {
      const { container } = renderSidebar({ isMobile: true });
      const row = container.querySelector<HTMLElement>('[data-thread-row="1"]')!;
      fireEvent.pointerDown(row, { pointerId: 1, button: 0, clientX: 0, clientY: 0 });
      act(() => {
        vi.advanceTimersByTime(LONG_PRESS_MS + 10);
      });
      expect(container.querySelector("[data-thread-menu]")).toBeTruthy();
      // 同一份菜单：标签项也在。
      expect(screen.getByRole("menuitemradio", { name: "红" })).toBeTruthy();
    } finally {
      vi.useRealTimers();
    }
  });
});

const aside = (container: HTMLElement) => container.querySelector("aside")!;

describe("ThreadSidebar width", () => {
  it("renders the drag handle with separator semantics", () => {
    const { container } = renderSidebar();
    const handle = container.querySelector('[role="separator"]')!;
    expect(handle).not.toBeNull();
    expect(handle.getAttribute("aria-orientation")).toBe("vertical");
  });

  it("defaults to the default width", () => {
    const { container } = renderSidebar();
    expect(aside(container).style.width).toBe(`${DEFAULT_SIDEBAR_WIDTH}px`);
  });

  it("restores the persisted width on mount", () => {
    localStorage.setItem(SIDEBAR_WIDTH_STORAGE_KEY, "320");
    const { container } = renderSidebar();
    expect(aside(container).style.width).toBe("320px");
  });
});

const handle = (container: HTMLElement) =>
  container.querySelector<HTMLElement>('[role="separator"]')!;

function drag(container: HTMLElement, toClientX: number, fromClientX = 0) {
  fireEvent.mouseDown(handle(container), { clientX: fromClientX });
  fireEvent.mouseMove(document, { clientX: toClientX });
  fireEvent.mouseUp(document);
}

describe("ThreadSidebar drag-resize", () => {
  afterEach(() => {
    document.body.style.cursor = "";
    document.body.style.userSelect = "";
  });

  it("grows the sidebar when dragging right", () => {
    const { container } = renderSidebar();
    drag(container, 100);
    expect(aside(container).style.width).toBe(`${DEFAULT_SIDEBAR_WIDTH + 100}px`);
  });

  it("clamps to MIN when dragging far left", () => {
    const { container } = renderSidebar();
    drag(container, -10000);
    expect(aside(container).style.width).toBe(`${MIN_SIDEBAR_WIDTH}px`);
  });

  it("clamps to MAX when dragging far right", () => {
    const { container } = renderSidebar();
    drag(container, 10000);
    expect(aside(container).style.width).toBe(`${MAX_SIDEBAR_WIDTH}px`);
  });

  it("stops resizing after mouseup", () => {
    const { container } = renderSidebar();
    drag(container, 50);
    fireEvent.mouseMove(document, { clientX: 400 });
    expect(aside(container).style.width).toBe(`${DEFAULT_SIDEBAR_WIDTH + 50}px`);
  });
});

describe("ThreadSidebar persistence", () => {
  it("saves the clamped width on mouseup", () => {
    const { container } = renderSidebar();
    drag(container, 10000); // clamps to MAX
    expect(localStorage.getItem(SIDEBAR_WIDTH_STORAGE_KEY)).toBe(String(MAX_SIDEBAR_WIDTH));
  });

  it("does not write during the drag (only on release)", () => {
    const { container } = renderSidebar();
    fireEvent.mouseDown(handle(container), { clientX: 0 });
    fireEvent.mouseMove(document, { clientX: 80 });
    expect(localStorage.getItem(SIDEBAR_WIDTH_STORAGE_KEY)).toBeNull();
    fireEvent.mouseUp(document);
    expect(localStorage.getItem(SIDEBAR_WIDTH_STORAGE_KEY)).toBe(
      String(DEFAULT_SIDEBAR_WIDTH + 80),
    );
  });

  it("restores a width dragged to MIN on the next mount", () => {
    const first = renderSidebar();
    drag(first.container, -10000);
    first.unmount();
    const second = renderSidebar();
    expect(aside(second.container).style.width).toBe(`${MIN_SIDEBAR_WIDTH}px`);
  });
});

describe("ThreadSidebar drag hygiene", () => {
  afterEach(() => {
    document.body.style.cursor = "";
    document.body.style.userSelect = "";
  });

  it("sets the body cursor during a drag and restores it on release", () => {
    const { container } = renderSidebar();
    fireEvent.mouseDown(handle(container), { clientX: 0 });
    expect(document.body.style.cursor).toBe("col-resize");
    fireEvent.mouseUp(document);
    expect(document.body.style.cursor).toBe("");
  });

  it("cleans up listeners and cursor when unmounted mid-drag", () => {
    const { container, unmount } = renderSidebar();
    fireEvent.mouseDown(handle(container), { clientX: 0 });
    unmount();
    expect(document.body.style.cursor).toBe("");
    // A stray move after unmount must not throw or resurrect the drag.
    expect(() => fireEvent.mouseMove(document, { clientX: 9999 })).not.toThrow();
    expect(document.body.style.cursor).toBe("");
  });

  it("does not leave the cursor stuck after a second mousedown mid-drag", () => {
    const { container } = renderSidebar();
    fireEvent.mouseDown(handle(container), { clientX: 0 });
    fireEvent.mouseDown(handle(container), { clientX: 10 });
    fireEvent.mouseUp(document);
    expect(document.body.style.cursor).toBe("");
  });

  it("ends the drag and persists the width when the window blurs", () => {
    const { container } = renderSidebar();
    fireEvent.mouseDown(handle(container), { clientX: 0 });
    fireEvent.mouseMove(document, { clientX: 80 });
    fireEvent.blur(window);
    expect(document.body.style.cursor).toBe("");
    expect(localStorage.getItem(SIDEBAR_WIDTH_STORAGE_KEY)).toBe(
      String(DEFAULT_SIDEBAR_WIDTH + 80),
    );
  });
});

describe("ThreadSidebar status", () => {
  it("shows a spinning badge for a running thread", () => {
    const { container } = renderSidebar({ statuses: new Map([["1", "running"]]) });
    const badge = container.querySelector('[aria-label="Thread status"]')!;
    expect(badge.getAttribute("data-status")).toBe("running");
    // jsdom has no Web Animations API, so we fall back to the CSS `animate-spin`.
    // (Where WAAPI exists the spin is driven by `useSpinPhase` instead; see the
    // reorder test below.)
    expect(typeof Element.prototype.animate).toBe("undefined");
    expect(badge.className).toContain("animate-spin");
  });

  it("drives the spin with WAAPI (no CSS class) when the API is available", () => {
    const created: Element[] = [];
    (Element.prototype as unknown as { animate?: unknown }).animate = function (this: Element) {
      created.push(this);
      return { currentTime: 0, cancel() {} } as unknown as Animation;
    };
    try {
      const { container } = renderSidebar({ statuses: new Map([["1", "running"]]) });
      const badge = container.querySelector('[aria-label="Thread status"]')!;
      expect(created).toContain(badge);
      // Exactly one driver: the WAAPI animation, not also the CSS class.
      expect(badge.className).not.toContain("animate-spin");
    } finally {
      delete (Element.prototype as unknown as { animate?: unknown }).animate;
    }
  });

  it("shows an amber badge for an awaiting-approval thread", () => {
    const { container } = renderSidebar({ statuses: new Map([["1", "awaiting_approval"]]) });
    const badge = container.querySelector('[aria-label="Thread status"]')!;
    expect(badge.getAttribute("data-status")).toBe("awaiting_approval");
    expect(badge.className).toContain("bg-amber-400");
    // The badge keeps rotating across states (the animation is not dropped and
    // re-added, which would restart it): the CSS fallback class stays applied.
    expect(badge.className).toContain("animate-spin");
  });

  it("shows an unread dot and clears it when the thread is current", () => {
    const { container } = renderSidebar({
      unread: new Map([["1", "completed"]]),
      currentId: null,
    });
    expect(container.querySelector('[aria-label="Unread"]')).not.toBeNull();

    const shown = renderSidebar({ unread: new Map([["1", "completed"]]), currentId: "1" });
    expect(shown.container.querySelector('[aria-label="Unread"]')).toBeNull();
  });

  it("allows selecting a thread while another one is running", () => {
    const onSelect = vi.fn();
    renderSidebar({
      statuses: new Map([["1", "running"]]),
      onSelect,
    });
    fireEvent.click(screen.getByText("beta-thread"));
    expect(onSelect).toHaveBeenCalledWith("2");
  });

  it("colors the unread dot by the turn outcome", () => {
    const completed = renderSidebar({
      unread: new Map([["1", "completed"]]),
      currentId: null,
    });
    expect(completed.container.querySelector('[aria-label="Unread"]')!.className).toContain(
      "bg-blue-400",
    );

    const failed = renderSidebar({ unread: new Map([["1", "failed"]]), currentId: null });
    expect(failed.container.querySelector('[aria-label="Unread"]')!.className).toContain(
      "bg-red-400",
    );

    const interrupted = renderSidebar({
      unread: new Map([["1", "interrupted"]]),
      currentId: null,
    });
    expect(interrupted.container.querySelector('[aria-label="Unread"]')!.className).toContain(
      "bg-fg-muted",
    );
  });

  it("keeps the running badge mounted across a status flip", () => {
    const props = sidebarProps({ statuses: new Map([["1", "running"]]) });
    const { container, rerender } = render(<ThreadSidebar {...props} />);
    const running = container.querySelector('[aria-label="Thread status"]')!;
    expect(running).not.toBeNull();

    // A tool call asks for approval, then the user allows it: running →
    // awaiting_approval → running. Unmounting the badge in between restarts its
    // CSS animation, which reads as "plays a short segment then loops".
    rerender(<ThreadSidebar {...props} statuses={new Map([["1", "awaiting_approval"]])} />);
    expect(container.querySelector('[aria-label="Thread status"]')!.getAttribute("data-status")).toBe(
      "awaiting_approval",
    );

    rerender(<ThreadSidebar {...props} statuses={new Map([["1", "running"]])} />);
    expect(container.querySelector('[aria-label="Thread status"]')).toBe(running);
  });

  it("does not restart the running badge's animation when the list reorders", () => {
    // Root cause of the "回退" (rewind): the sidebar is server-sorted by
    // `updated_at` desc, so when any thread's `updated_at` bumps this row is MOVED
    // in the DOM (React applies a keyed reorder as `insertBefore`). A CSS animation
    // restarts from `currentTime = 0` on any such move — snapping the ring back to
    // 12 o'clock — while a Web-Animations animation does not. We drive the spin
    // with WAAPI, so a reorder must not create a new animation.
    const created: { el: Element; currentTime: number }[] = [];
    (Element.prototype as unknown as { animate?: unknown }).animate = function (this: Element) {
      const rec = { el: this, currentTime: 0 };
      created.push(rec);
      return {
        get currentTime() {
          return rec.currentTime;
        },
        set currentTime(v: number) {
          rec.currentTime = v;
        },
        cancel() {},
      } as unknown as Animation;
    };
    try {
      const props = sidebarProps({
        groups: [
          {
            workspace: "/work/projA",
            exists: true,
            threads: [thread("1", "alpha-thread", "/work/projA"), thread("3", "gamma-thread", "/work/projA")],
          },
        ],
        statuses: new Map([["1", "running"]]),
      });
      const { container, rerender } = render(<ThreadSidebar {...props} />);
      const badge = container.querySelector('[aria-label="Thread status"]')!;
      expect(badge).not.toBeNull();
      expect(created).toHaveLength(1);
      const first = created[0].el;

      // The server now returns the other thread first (its `updated_at` bumped).
      rerender(
        <ThreadSidebar
          {...props}
          groups={[
            {
              workspace: "/work/projA",
              exists: true,
              threads: [thread("3", "gamma-thread", "/work/projA"), thread("1", "alpha-thread", "/work/projA")],
            },
          ]}
        />,
      );

      // The row was moved (order flipped) but the badge element is the same node
      // and NO new animation was started — the timeline survived the reorder.
      expect(container.querySelector('[aria-label="Thread status"]')).toBe(badge);
      expect(created).toHaveLength(1);
      expect(created[0].el).toBe(first);
    } finally {
      delete (Element.prototype as unknown as { animate?: unknown }).animate;
    }
  });

  it("renders no status badge for an idle thread", () => {
    const { container } = renderSidebar({ statuses: new Map([["1", "idle"]]) });
    expect(container.querySelector('[aria-label="Thread status"]')).toBeNull();
  });
});

/**
 * 看板条目：只给已登记的项目显示，右键菜单按登记状态二选一。
 *
 * 用例自带一份最小 groups，避免改动上面共享的 fixture（那会让既有断言跟着
 * 漂移）。路径用 /proj，与计划里的验收片段一致。
 */
const boardGroups: WorkspaceGroup[] = [
  { workspace: "/proj", exists: true, threads: [thread("b1", "board-thread", "/proj")] },
];

/** 在某个工作区分组头上打开右键菜单（与键盘路径同一条处理链）。 */
function openWorkspaceMenu(container: HTMLElement, workspace: string) {
  const title = container.querySelector(`[title="${workspace}"]`)!;
  const header = title.closest<HTMLElement>("div[tabindex]")!;
  fireEvent.contextMenu(header);
}

describe("ThreadSidebar 看板", () => {
  it("项目右键菜单提供创建看板", () => {
    const onCreateBoard = vi.fn();
    const { container } = renderSidebar({ groups: boardGroups, boards: [], onCreateBoard });

    openWorkspaceMenu(container, "/proj");
    const item = screen.getByText("创建 Superpowers 看板");
    fireEvent.click(item);

    expect(onCreateBoard).toHaveBeenCalledWith("/proj");
  });

  it("已登记的项目右键菜单改为移除看板，且不再提供创建", () => {
    const onRemoveBoard = vi.fn();
    const onCreateBoard = vi.fn();
    const { container } = renderSidebar({
      groups: boardGroups,
      boards: ["/proj"],
      onCreateBoard,
      onRemoveBoard,
    });

    openWorkspaceMenu(container, "/proj");
    expect(screen.queryByText("创建 Superpowers 看板")).toBeNull();
    fireEvent.click(screen.getByText("移除看板"));

    expect(onRemoveBoard).toHaveBeenCalledWith("/proj");
    expect(onCreateBoard).not.toHaveBeenCalled();
  });

  it("已登记的项目显示看板条目并可打开", () => {
    const onOpenBoard = vi.fn();
    const { container } = renderSidebar({
      groups: boardGroups,
      boards: ["/proj"],
      boardSummaries: { "/proj": "2 排队 · 1 运行中" },
      selectedBoard: null,
      onOpenBoard,
    });

    // aria-label 与可见文字都是「看板」，条目本身带完整路径的 title。
    const entry = container.querySelector<HTMLElement>('[aria-label="看板"]')!;
    expect(entry.getAttribute("title")).toBe("/proj");
    expect(entry.textContent).toContain("2 排队 · 1 运行中");

    fireEvent.click(screen.getByText("看板"));
    expect(onOpenBoard).toHaveBeenCalledWith("/proj");
  });

  it("未登记的项目没有看板条目", () => {
    const { container } = renderSidebar({ groups: boardGroups, boards: [] });
    expect(screen.queryByText("看板")).toBeNull();
    expect(container.querySelector('[aria-label="看板"]')).toBeNull();
  });

  it("只给已登记的那个项目画条目", () => {
    const onOpenBoard = vi.fn();
    const groups2: WorkspaceGroup[] = [
      ...boardGroups,
      { workspace: "/other", exists: true, threads: [thread("o1", "other-thread", "/other")] },
    ];
    const { container } = renderSidebar({
      groups: groups2,
      boards: ["/other"],
      boardSummaries: { "/other": "空" },
      onOpenBoard,
    });

    const entries = container.querySelectorAll('[aria-label="看板"]');
    expect(entries).toHaveLength(1);
    expect(entries[0].getAttribute("title")).toBe("/other");

    fireEvent.click(screen.getByText("看板"));
    expect(onOpenBoard).toHaveBeenCalledWith("/other");
  });

  it("选中的看板条目用与线程一致的底色", () => {
    const selected = renderSidebar({ groups: boardGroups, boards: ["/proj"], selectedBoard: "/proj" });
    const selectedClasses = selected.container
      .querySelector('[aria-label="看板"]')!
      .className.split(/\s+/);
    expect(selectedClasses).toContain("bg-neutral-800");

    const idle = renderSidebar({ groups: boardGroups, boards: ["/proj"], selectedBoard: null });
    const idleClasses = idle.container.querySelector('[aria-label="看板"]')!.className.split(/\s+/);
    // 未选中只有 hover 底色；按 token 比较，免得 hover:bg-neutral-800/50 被当选中态。
    expect(idleClasses).not.toContain("bg-neutral-800");
  });

  it("把卡片会话收进默认折叠的「看板会话」小节，展开后可见", () => {
    const withCard: WorkspaceGroup[] = [
      {
        workspace: "/proj",
        exists: true,
        threads: [
          thread("t-plain", "plain-thread", "/proj"),
          { ...thread("t-card", "看板 · card-1", "/proj"), card_id: "card-1", board_project: "/proj" },
        ],
      },
    ];
    const { container } = renderSidebar({ groups: withCard, boards: ["/proj"] });

    // 小节存在，但默认折叠 → 卡片行不可见，普通行仍在。
    expect(screen.getByLabelText("看板会话")).toBeTruthy();
    expect(container.textContent).not.toContain("看板 · card-1");
    expect(container.textContent).toContain("plain-thread");

    fireEvent.click(screen.getByLabelText("看板会话"));
    expect(container.textContent).toContain("看板 · card-1");
  });

  it("看板会话行比项目组直属会话行多缩进一档", () => {
    const withCard: WorkspaceGroup[] = [
      {
        workspace: "/proj",
        exists: true,
        threads: [
          thread("t-plain", "plain-thread", "/proj"),
          { ...thread("t-card", "看板 · card-1", "/proj"), card_id: "card-1", board_project: "/proj" },
        ],
      },
    ];
    const { container } = renderSidebar({ groups: withCard, boards: ["/proj"] });
    fireEvent.click(screen.getByLabelText("看板会话"));

    const rows = Array.from(container.querySelectorAll("[data-group-row]"));
    const plain = rows.find((r) => r.textContent!.includes("plain-thread"))!;
    const card = rows.find((r) => r.textContent!.includes("看板 · card-1"))!;
    // 直属行 pl-3；卡片会话 pl-7，缩进一档让层级可读。
    expect(plain.className).toContain("pl-3");
    expect(card.className).toContain("pl-7");
  });

  it("没有卡片会话时不渲染「看板会话」小节", () => {
    const { container } = renderSidebar({ groups: boardGroups, boards: ["/proj"] });
    expect(container.querySelector('[aria-label="看板会话"]')).toBeNull();
    expect(container.textContent).toContain("board-thread");
  });

  it("看板被移除后卡片会话仍留在小节里（不整批消失）", () => {
    const withCard: WorkspaceGroup[] = [
      {
        workspace: "/proj",
        exists: true,
        threads: [
          { ...thread("t-card", "看板 · card-1", "/proj"), card_id: "card-1", board_project: "/proj" },
        ],
      },
    ];
    // boards 为空 = 该项目的看板已被移除。
    const { container } = renderSidebar({ groups: withCard, boards: [] });
    expect(screen.getByLabelText("看板会话")).toBeTruthy();
    fireEvent.click(screen.getByLabelText("看板会话"));
    expect(container.textContent).toContain("看板 · card-1");
  });
});

describe("ThreadSidebar pinned section", () => {
  it("renders pinned threads in the Pinned section, in the given order", () => {
    const { container } = renderSidebar({ pinned: [pinnedThread("9", "pinned-nine"), pinnedThread("8", "pinned-eight")] });
    expect(container.textContent).toContain("Pinned");
    const rows = Array.from(container.querySelectorAll("[data-pinned-row]"));
    expect(rows.map((r) => r.textContent)).toEqual([
      expect.stringContaining("pinned-nine"),
      expect.stringContaining("pinned-eight"),
    ]);
  });

  it("hides pinned threads from their workspace group", () => {
    const groups: WorkspaceGroup[] = [
      { workspace: "/work/projA", exists: true, threads: [thread("1", "alpha", "/work/projA"), pinnedThread("9", "pin-a")] },
    ];
    const { container } = renderSidebar({ groups, pinned: [pinnedThread("9", "pin-a")] });
    const groupRows = Array.from(container.querySelectorAll("[data-group-row]")).map((r) => r.textContent!);
    expect(groupRows.some((t) => t.includes("pin-a"))).toBe(false);
    expect(groupRows.some((t) => t.includes("alpha"))).toBe(true);
  });

  it("does not render a Pinned section when there are no pinned threads", () => {
    const { container } = renderSidebar({ pinned: [] });
    expect(container.textContent).not.toContain("Pinned");
  });

  it("toggles pin on button click without selecting the thread", () => {
    const onTogglePin = vi.fn();
    const onSelect = vi.fn();
    const { container } = renderSidebar({ groups, onTogglePin, onSelect });
    const btn = container.querySelector<HTMLButtonElement>('[aria-label="Pin thread"]')!;
    fireEvent.click(btn);
    expect(onTogglePin).toHaveBeenCalledWith("1", true);
    expect(onSelect).not.toHaveBeenCalled();
  });

  it("shows a lit pin button on an already-pinned row", () => {
    const onTogglePin = vi.fn();
    const { container } = renderSidebar({
      pinned: [pinnedThread("9", "pin-a")],
      onTogglePin,
    });
    // 分组里 alpha-thread 未置顶 → 该行是 "Pin thread";置顶行是 "Unpin thread"。
    const lit = container.querySelector<HTMLButtonElement>('[aria-label="Unpin thread"]')!;
    expect(lit.getAttribute("aria-pressed")).toBe("true");
    // 未置顶的行（默认 fixtures 里的 alpha/beta）仍提供可点的 "Pin thread"。
    expect(container.querySelectorAll('[aria-label="Pin thread"]')).toHaveLength(2);
    // 置顶状态在同一时刻只能有一处点亮：只有置顶行是 aria-pressed="true"。
    expect(container.querySelectorAll('[aria-pressed="true"]')).toHaveLength(1);
    fireEvent.click(lit);
    expect(onTogglePin).toHaveBeenCalledWith("9", false);
  });

  it("reorders pinned threads on drop", () => {
    const onReorderPinned = vi.fn();
    const { container } = renderSidebar({
      pinned: [pinnedThread("9", "nine"), pinnedThread("8", "eight")],
      onReorderPinned,
    });
    const rows = container.querySelectorAll<HTMLElement>("[data-pinned-row]");
    // 把第一行拖到第二行位置。
    fireEvent.dragStart(rows[0]);
    fireEvent.dragOver(rows[1]);
    // 悬停期间要有可见的落点指示线，否则用户看不到会落到哪儿。
    expect(container.querySelector("[data-pin-drop-indicator]")).not.toBeNull();
    fireEvent.drop(rows[1]);
    expect(onReorderPinned).toHaveBeenCalledWith(["8", "9"]);
  });

  it("does not reorder when a pinned row is dropped on itself", () => {
    const onReorderPinned = vi.fn();
    const { container } = renderSidebar({
      pinned: [pinnedThread("9", "nine"), pinnedThread("8", "eight")],
      onReorderPinned,
    });
    const rows = container.querySelectorAll<HTMLElement>("[data-pinned-row]");
    fireEvent.dragStart(rows[0]);
    fireEvent.dragOver(rows[0]);
    fireEvent.drop(rows[0]);
    expect(onReorderPinned).not.toHaveBeenCalled();
  });

  it("collapses and expands the pinned section from its caret", () => {
    const { container } = renderSidebar({ pinned: [pinnedThread("9", "nine")] });
    fireEvent.click(container.querySelector('[aria-label="Collapse pinned"]')!);
    expect(container.querySelectorAll("[data-pinned-row]")).toHaveLength(0);
    // 折叠是局部的:工作区分组不受影响。
    expect(container.textContent).toContain("alpha-thread");

    fireEvent.click(container.querySelector('[aria-label="Expand pinned"]')!);
    expect(container.querySelectorAll("[data-pinned-row]")).toHaveLength(1);
  });

  it("renders neither a pin button nor draggable on the row being renamed", () => {
    const { container } = renderSidebar({ pinned: [pinnedThread("9", "nine")] });
    fireEvent.doubleClick(screen.getByText("nine"));
    const row = container.querySelector<HTMLElement>("[data-pinned-row]")!;
    expect(row.querySelector("[aria-label=\"Unpin thread\"]")).toBeNull();
    expect(row.querySelector("[aria-label=\"Pin thread\"]")).toBeNull();
    expect(row.getAttribute("draggable")).not.toBe("true");
  });

  it("does not make unpinned group rows draggable", () => {
    const { container } = renderSidebar();
    const notDraggable = Array.from(container.querySelectorAll<HTMLElement>("[data-group-row]")).every(
      (r) => r.getAttribute("draggable") !== "true",
    );
    expect(notDraggable).toBe(true);
  });

  it("offers a settings button in the footer", () => {
    const onOpenSettings = vi.fn();
    renderSidebar({ onOpenSettings });
    fireEvent.click(screen.getByRole("button", { name: "设置" }));
    expect(onOpenSettings).toHaveBeenCalledTimes(1);
  });

  it("spawns a ripple at the pointer position on the New-thread button", () => {
    const { container } = renderSidebar({
      workspaces: [{ path: "/a/alpha", exists: true }],
    });
    const button = newThreadTrigger(container);

    // jsdom 的 getBoundingClientRect 恒为 0，因此换算后的相对坐标就等于 clientX/Y。
    fireEvent.pointerDown(button, { clientX: 12, clientY: 7 });

    const ripples = button.querySelectorAll("[data-ripple]");
    expect(ripples).toHaveLength(1);
    const ripple = ripples[0] as HTMLElement;
    expect(ripple.style.left).toBe("12px");
    expect(ripple.style.top).toBe("7px");
  });

  it("removes a ripple after its animation ends", () => {
    const { container } = renderSidebar({
      workspaces: [{ path: "/a/alpha", exists: true }],
    });
    const button = newThreadTrigger(container);
    fireEvent.pointerDown(button, { clientX: 4, clientY: 4 });

    const ripple = button.querySelector("[data-ripple]")!;
    fireEvent.animationEnd(ripple);
    expect(button.querySelector("[data-ripple]")).toBeNull();
  });

  it("keeps the click behaviour intact alongside the ripple", () => {
    const onBrowse = vi.fn();
    const { container } = renderSidebar({ workspaces: [], onBrowse });
    const button = newThreadTrigger(container);

    // 无最近目录：点击仍然直接走原生选择器，涟漪不改变这条路径。
    fireEvent.pointerDown(button, { clientX: 2, clientY: 2 });
    fireEvent.click(button);
    expect(onBrowse).toHaveBeenCalledTimes(1);
    expect(button.querySelectorAll("[data-ripple]")).toHaveLength(1);
  });
});

/**
 * 手机端长按（= 右键）。落点由 hook 委托判定，故这里直接对行/组头做 pointerdown，
 * 再用 fake timers 推过阈值。
 *
 * 推进必须用 `act(async () => await vi.advanceTimersByTimeAsync(...))`：同步的
 * `advanceTimersByTime` 不 flush React 更新，会看到菜单还没渲染。
 */
async function longPress(el: Element) {
  await act(async () => {
    fireEvent.pointerDown(el, { clientX: 5, clientY: 5, button: 0 });
    await vi.advanceTimersByTimeAsync(LONG_PRESS_MS);
  });
}

describe("ThreadSidebar 手机长按", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it("长按会话行打开会话菜单且不选中会话", async () => {
    vi.useFakeTimers();
    const onSelect = vi.fn();
    const { container } = renderSidebar({ isMobile: true, onSelect });

    await longPress(screen.getByText("alpha-thread"));

    expect(container.querySelector('[data-thread-menu=""]')).not.toBeNull();
    expect(screen.getByText("重命名")).toBeTruthy();
    expect(screen.getByText("删除")).toBeTruthy();
    expect(onSelect).not.toHaveBeenCalled();
  });

  it("长按会话行的「重命名」进入编辑态", async () => {
    vi.useFakeTimers();
    const { container } = renderSidebar({ isMobile: true });

    await longPress(screen.getByText("alpha-thread"));
    fireEvent.click(screen.getByText("重命名"));

    expect(container.querySelector("[data-rename-input]")).not.toBeNull();
    expect(container.querySelector('[data-thread-menu=""]')).toBeNull();
  });

  it("长按会话行的「置顶」切换置顶，且不选中会话", async () => {
    vi.useFakeTimers();
    const onTogglePin = vi.fn();
    const onSelect = vi.fn();
    renderSidebar({ isMobile: true, onTogglePin, onSelect });

    await longPress(screen.getByText("alpha-thread"));
    fireEvent.click(screen.getByText("置顶"));

    expect(onTogglePin).toHaveBeenCalledWith("1", true);
    // 菜单渲染在行 div 内部，而行 div 自己 onClick=onSelect；点菜单项不能连带选中。
    expect(onSelect).not.toHaveBeenCalled();
  });

  it("已置顶的行菜单显示「取消置顶」", async () => {
    vi.useFakeTimers();
    const onTogglePin = vi.fn();
    renderSidebar({ isMobile: true, pinned: [pinnedThread("9", "pin-a")], onTogglePin });

    await longPress(screen.getByText("pin-a"));
    fireEvent.click(screen.getByText("取消置顶"));

    expect(onTogglePin).toHaveBeenCalledWith("9", false);
  });

  it("长按会话行的「删除」调用 onDelete", async () => {
    // 组件的「删除」菜单项直调 onDelete；「删前确认」在 App.tsx 的 deleteThread 里
    // （服务端答 needs_confirmation 时才 window.confirm），由 App.test.tsx 覆盖。
    vi.useFakeTimers();
    const onDelete = vi.fn();
    renderSidebar({ isMobile: true, onDelete });

    await longPress(screen.getByText("alpha-thread"));
    fireEvent.click(screen.getByText("删除"));

    expect(onDelete).toHaveBeenCalledWith("1");
  });

  it("点 backdrop 关闭会话菜单且不选中会话", async () => {
    vi.useFakeTimers();
    const onSelect = vi.fn();
    const { container } = renderSidebar({ isMobile: true, onSelect });

    await longPress(screen.getByText("alpha-thread"));
    expect(container.querySelector('[data-thread-menu=""]')).not.toBeNull();

    // 用标记选择 backdrop：它同时锁住「backdrop 存在该标记」这条契约。
    // 缺了标记，行上的吞 click 会把这次点击一起吞掉，菜单关不掉。
    fireEvent.click(container.querySelector("[data-thread-menu-overlay]")!);

    expect(container.querySelector('[data-thread-menu=""]')).toBeNull();
    expect(onSelect).not.toHaveBeenCalled();
  });

  it("长按组头打开工作区菜单", async () => {
    vi.useFakeTimers();
    const { container } = renderSidebar({ isMobile: true });

    const header = container.querySelector<HTMLElement>("[data-ws-header]")!;
    await longPress(header);

    const menu = container.querySelector('[role="menu"]');
    expect(menu).not.toBeNull();
    expect(menu!.textContent).toContain("New thread here");
    expect(menu!.textContent).toContain("Remove from list");
  });

  it("Escape 关闭会话菜单", async () => {
    vi.useFakeTimers();
    const { container } = renderSidebar({ isMobile: true });

    await longPress(screen.getByText("alpha-thread"));
    expect(container.querySelector('[data-thread-menu=""]')).not.toBeNull();

    fireEvent.keyDown(document, { key: "Escape" });
    expect(container.querySelector('[data-thread-menu=""]')).toBeNull();
  });

  it("桌面端（未传 isMobile）长按不开任何菜单", async () => {
    vi.useFakeTimers();
    const { container } = renderSidebar();

    await longPress(screen.getByText("alpha-thread"));

    expect(container.querySelector('[data-thread-menu=""]')).toBeNull();
    expect(container.querySelector('[role="menu"]')).toBeNull();
  });
});

describe("ThreadSidebar 搜索", () => {
  const searchBox = () => screen.getByLabelText("搜索会话") as HTMLInputElement;

  it("手机端：搜索框与会话行同属滚动容器（随列表滚动）", () => {
    const { container } = renderSidebar({ isMobile: true });
    const row = container.querySelector("[data-group-row]")!;
    const scroller = row.closest(".overflow-y-auto");
    expect(scroller).not.toBeNull();
    expect(searchBox().closest(".overflow-y-auto")).toBe(scroller);
  });

  it("桌面端：搜索框在任何滚动容器之外（钉在列表上方）", () => {
    const { container } = renderSidebar();
    const row = container.querySelector("[data-group-row]")!;
    expect(row.closest(".overflow-y-auto")).not.toBeNull();
    expect(searchBox().closest(".overflow-y-auto")).toBeNull();
  });

  it("为滚动会话内容预留右侧滚动条安全间距", () => {
    const { container } = renderSidebar();
    const row = container.querySelector("[data-group-row]")!;
    const content = row.closest("[data-thread-list-content]");
    const scroller = row.closest(".overflow-y-auto");

    expect(content).not.toBeNull();
    expect(content!.className.split(/\s+/)).toContain("pr-4");
    expect(content!.parentElement).toBe(scroller);
  });

  it("搜索命中藏在「看板会话」折叠小节里时，该小节搜索期间自动展开", () => {
    const withCard: WorkspaceGroup[] = [
      {
        workspace: "/proj",
        exists: true,
        threads: [
          { ...thread("t-card", "看板 · card-1", "/proj"), card_id: "card-1", board_project: "/proj" },
        ],
      },
    ];
    const { container } = renderSidebar({ groups: withCard, boards: ["/proj"] });
    // 默认折叠：空闲时命中行不可见。
    expect(container.textContent).not.toContain("看板 · card-1");
    fireEvent.change(searchBox(), { target: { value: "card-1" } });
    // 搜索期间小节自动展开：命中行可见。
    expect(container.textContent).toContain("看板 · card-1");
    fireEvent.change(searchBox(), { target: { value: "" } });
    // 清空后恢复折叠。
    expect(container.textContent).not.toContain("看板 · card-1");
  });

  it("搜索栏在 New thread 之后、会话列表之前", () => {
    const { container } = renderSidebar();
    const newThread = newThreadTrigger(container);
    const rows = container.querySelector('[data-group-row]')!;
    // 4 = DOCUMENT_POSITION_FOLLOWING
    expect(newThread.compareDocumentPosition(searchBox()) & 4).toBe(4);
    expect(searchBox().compareDocumentPosition(rows) & 4).toBe(4);
  });

  it("输入关键词后只留命中的会话与分组", () => {
    const { container } = renderSidebar();
    fireEvent.change(searchBox(), { target: { value: "beta" } });
    expect(container.textContent).toContain("beta-thread");
    expect(container.textContent).not.toContain("alpha-thread");
    // alpha 所在的 /work/projA 一行不剩 → 组头一并消失。
    expect(container.textContent).not.toContain("projA");
    expect(container.textContent).toContain("projB");
  });

  it("目录名命中时带出该目录全部会话", () => {
    const { container } = renderSidebar();
    fireEvent.change(searchBox(), { target: { value: "projA" } });
    expect(container.textContent).toContain("alpha-thread");
    expect(container.textContent).not.toContain("beta-thread");
  });

  it("无匹配时显示「无匹配会话」", () => {
    const { container } = renderSidebar();
    fireEvent.change(searchBox(), { target: { value: "zzz-nothing" } });
    expect(container.textContent).toContain("无匹配会话");
  });

  it("列表本身为空且正在搜索时，显示「无匹配会话」而不是「选择一个目录开始」", () => {
    // 这条锁住空态判据里的 `!searching` 那一半：没有它，一个空列表在搜索时
    // 会同时冒出「选择一个目录开始」，与「无匹配会话」互相打脸。
    const { container } = renderSidebar({ groups: [], pinned: [] });
    expect(container.textContent).toContain("选择一个目录开始");

    fireEvent.change(searchBox(), { target: { value: "anything" } });
    expect(container.textContent).toContain("无匹配会话");
    expect(container.textContent).not.toContain("选择一个目录开始");
  });

  it("折叠的分组在搜索期间自动展开，清空后恢复折叠", () => {
    const { container } = renderSidebar();
    // 先折叠 projA（第一个 Collapse 按钮属于它）。
    fireEvent.click(container.querySelector('[aria-label="Collapse"]')!);
    expect(container.textContent).not.toContain("alpha-thread");

    fireEvent.change(searchBox(), { target: { value: "alpha" } });
    expect(container.textContent).toContain("alpha-thread");

    fireEvent.change(searchBox(), { target: { value: "" } });
    expect(container.textContent).not.toContain("alpha-thread");
  });

  it("搜索期间隐藏「看板」条目行", () => {
    const { container } = renderSidebar({ groups: boardGroups, boards: ["/proj"] });
    expect(container.querySelector('[aria-label="看板"]')).not.toBeNull();
    fireEvent.change(searchBox(), { target: { value: "proj" } });
    expect(container.querySelector('[aria-label="看板"]')).toBeNull();
  });

  it("清除按钮仅在有内容时出现，点击后恢复完整列表", () => {
    const { container } = renderSidebar();
    expect(screen.queryByLabelText("清除搜索")).toBeNull();

    fireEvent.change(searchBox(), { target: { value: "beta" } });
    expect(screen.queryByLabelText("清除搜索")).not.toBeNull();
    expect(container.textContent).not.toContain("alpha-thread");

    fireEvent.click(screen.getByLabelText("清除搜索"));
    expect(searchBox().value).toBe("");
    expect(container.textContent).toContain("alpha-thread");
    expect(screen.queryByLabelText("清除搜索")).toBeNull();
  });

  it("Escape 清空查询", () => {
    const { container } = renderSidebar();
    fireEvent.change(searchBox(), { target: { value: "beta" } });
    fireEvent.keyDown(searchBox(), { key: "Escape" });
    expect(searchBox().value).toBe("");
    expect(container.textContent).toContain("alpha-thread");
  });

  it("搜索期间折叠控件被禁用，不会悄悄改掉清空后的折叠视图", () => {
    // 渲染在搜索期间忽略折叠态，折叠控件若仍可点，用户点了毫无反馈却写进了 Set，
    // 清空搜索后视图就被悄悄改掉——正是「清空即恢复」这条承诺的破口。
    const withCard: WorkspaceGroup[] = [
      {
        workspace: "/proj",
        exists: true,
        threads: [
          thread("t-plain", "plain-thread", "/proj"),
          { ...thread("t-card", "看板 · card-1", "/proj"), card_id: "card-1", board_project: "/proj" },
        ],
      },
    ];
    const { container } = renderSidebar({ groups: withCard, boards: ["/proj"] });
    fireEvent.change(searchBox(), { target: { value: "proj" } });

    const collapse = container.querySelector<HTMLButtonElement>('[aria-label="Collapse"]')!;
    const boardToggle = container.querySelector<HTMLButtonElement>('[aria-label="看板会话"]')!;
    // 默认态：项目组展开（caret 是 Collapse），看板小节折叠（aria-expanded=false）。
    expect(collapse.disabled).toBe(true);
    expect(boardToggle.disabled).toBe(true);
    expect(boardToggle.getAttribute("aria-expanded")).toBe("true");
    fireEvent.click(collapse);
    fireEvent.click(boardToggle);

    // 清空后回到搜索前的折叠态：项目组仍展开、看板小节仍折叠。
    // 若上面两次点击写进了 Set，二者之一就会翻转，这里即失败。
    fireEvent.change(searchBox(), { target: { value: "" } });
    expect(container.querySelector('[aria-label="Collapse"]')).not.toBeNull();
    expect(container.querySelector('[aria-label="看板会话"]')!.getAttribute("aria-expanded")).toBe("false");
  });

  it("搜索期间置顶分区处于展开态", () => {
    const { container } = renderSidebar({ pinned: [pinnedThread("9", "pin-a")] });
    fireEvent.click(container.querySelector('[aria-label="Collapse pinned"]')!);
    expect(container.querySelectorAll("[data-pinned-row]")).toHaveLength(0);

    fireEvent.change(searchBox(), { target: { value: "pin-a" } });
    expect(container.querySelectorAll("[data-pinned-row]")).toHaveLength(1);
    expect(screen.getByLabelText("Collapse pinned").hasAttribute("disabled")).toBe(true);

    fireEvent.change(searchBox(), { target: { value: "" } });
    expect(container.querySelectorAll("[data-pinned-row]")).toHaveLength(0);
  });

  it("搜索期间双击会话行，[data-rename-input] 仍指向该行输入框", () => {
    const { container } = renderSidebar();
    fireEvent.change(searchBox(), { target: { value: "projA" } });
    fireEvent.doubleClick(screen.getByText("alpha-thread"));
    expect(container.querySelector("[data-rename-input]")).not.toBeNull();
    expect(container.querySelectorAll("[data-rename-input]")).toHaveLength(1);
  });
});
