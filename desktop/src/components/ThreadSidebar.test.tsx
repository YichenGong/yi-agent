/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, fireEvent, cleanup, screen } from "@testing-library/react";
import type { ComponentProps } from "react";
import { ThreadSidebar } from "./ThreadSidebar";
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

function thread(id: string, title: string, cwd: string): ThreadSummary {
  return { thread_id: id, cwd, model: "m", created_at: 0, updated_at: 0, title };
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
    statuses: new Map(),
    unread: new Map(),
    onSelect: vi.fn(),
    onRename: vi.fn(),
    onDelete: vi.fn(),
    onNew: vi.fn(),
    onRemoveWorkspace: vi.fn(),
    onBrowse: vi.fn(),
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
    const field = container.querySelector<HTMLInputElement>("input")!;
    fireEvent.change(field, { target: { value: "中文名" } });

    // macOS WKWebView rebuilds the committing keydown after tearing down the
    // composition: isComposing is false by then, keyCode is forced to 229.
    fireEvent.keyDown(field, { key: "Enter", keyCode: 229 });
    expect(onRename).not.toHaveBeenCalled();
    expect(container.querySelector("input")).not.toBeNull();

    fireEvent.keyDown(field, { key: "Enter" });
    expect(onRename).toHaveBeenCalledWith("1", "中文名");
    expect(container.querySelector("input")).toBeNull();
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
    const header = container.querySelectorAll<HTMLElement>("div[tabindex]")[0];
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
    const header = container.querySelectorAll<HTMLElement>("div[tabindex]")[0];
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
    expect(badge.className).toContain("animate-spin");
  });

  it("shows an amber badge for an awaiting-approval thread", () => {
    const { container } = renderSidebar({ statuses: new Map([["1", "awaiting_approval"]]) });
    const badge = container.querySelector('[aria-label="Thread status"]')!;
    expect(badge.getAttribute("data-status")).toBe("awaiting_approval");
    expect(badge.className).toContain("bg-amber-400");
    // The animation must stay applied across states: dropping and re-adding it
    // recreates the CSS animation and resets its timeline.
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
      "bg-neutral-400",
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

  it("renders no status badge for an idle thread", () => {
    const { container } = renderSidebar({ statuses: new Map([["1", "idle"]]) });
    expect(container.querySelector('[aria-label="Thread status"]')).toBeNull();
  });
});
