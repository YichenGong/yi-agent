/** @vitest-environment jsdom */
import { describe, expect, it, vi, afterEach } from "vitest";
import { fireEvent, render, cleanup, screen, waitFor } from "@testing-library/react";
import { SuperpowersKanbanEnqueue } from "./SuperpowersKanbanEnqueue";

afterEach(() => {
  cleanup();
});

/** 这些用例只管「加入看板」那条路；预排队另有专门的用例。 */
const noopPrequeue = async () => {};

/**
 * 入队入口现在收在「＋」菜单里：先开菜单，再点菜单项。
 *
 * 两步而不是一步，是因为卡片列表才是看板首屏的主要内容，一条常驻的入队按钮
 * 不该占掉它。
 */
const openMenu = () => fireEvent.click(screen.getByRole("button", { name: "添加到看板" }));
const clickEnqueue = () => {
  openMenu();
  fireEvent.click(screen.getByRole("menuitem", { name: "加入看板" }));
};

describe("SuperpowersKanbanEnqueue", () => {
  it("enqueues once with both picked paths", async () => {
    const pickFile = vi
      .fn<() => Promise<string | null>>()
      .mockResolvedValueOnce("/p/a.spec.md")
      .mockResolvedValueOnce("/p/a.plan.md");
    const enqueue = vi.fn<() => Promise<void>>().mockResolvedValue(undefined);

    render(
      <SuperpowersKanbanEnqueue pickFile={pickFile} enqueue={enqueue} prequeue={noopPrequeue} />,
    );
    clickEnqueue();

    await waitFor(() => expect(enqueue).toHaveBeenCalledTimes(1));
    expect(enqueue).toHaveBeenCalledWith("/p/a.spec.md", "/p/a.plan.md");
  });

  it("picks the spec before the plan, in that order", async () => {
    const order: string[] = [];
    const pickFile = vi.fn<() => Promise<string | null>>(async () => {
      order.push(`pick-${order.length + 1}`);
      return order.length === 1 ? "/p/a.spec.md" : "/p/a.plan.md";
    });
    const enqueue = vi.fn(async () => {
      order.push("enqueue");
    });

    render(
      <SuperpowersKanbanEnqueue pickFile={pickFile} enqueue={enqueue} prequeue={noopPrequeue} />,
    );
    clickEnqueue();

    await waitFor(() => expect(order).toEqual(["pick-1", "pick-2", "enqueue"]));
  });

  it("stops without enqueuing when the spec pick is cancelled", async () => {
    const pickFile = vi.fn<() => Promise<string | null>>().mockResolvedValue(null);
    const enqueue = vi.fn<() => Promise<void>>().mockResolvedValue(undefined);

    render(
      <SuperpowersKanbanEnqueue pickFile={pickFile} enqueue={enqueue} prequeue={noopPrequeue} />,
    );
    clickEnqueue();

    await waitFor(() => expect(pickFile).toHaveBeenCalledTimes(1));
    expect(enqueue).not.toHaveBeenCalled();
  });

  it("stops without enqueuing when the plan pick is cancelled", async () => {
    const pickFile = vi
      .fn<() => Promise<string | null>>()
      .mockResolvedValueOnce("/p/a.spec.md")
      .mockResolvedValueOnce(null);
    const enqueue = vi.fn<() => Promise<void>>().mockResolvedValue(undefined);

    render(
      <SuperpowersKanbanEnqueue pickFile={pickFile} enqueue={enqueue} prequeue={noopPrequeue} />,
    );
    clickEnqueue();

    await waitFor(() => expect(pickFile).toHaveBeenCalledTimes(2));
    expect(enqueue).not.toHaveBeenCalled();
  });

  it("says the card is delivered and awaits validation, not that it is running", async () => {
    // A delivery only becomes a queued card on the plugin's next tick, so the
    // wording must not promise more than happened.
    const pickFile = vi
      .fn<() => Promise<string | null>>()
      .mockResolvedValueOnce("/p/a.spec.md")
      .mockResolvedValueOnce("/p/a.plan.md");
    const enqueue = vi.fn<() => Promise<void>>().mockResolvedValue(undefined);

    render(
      <SuperpowersKanbanEnqueue pickFile={pickFile} enqueue={enqueue} prequeue={noopPrequeue} />,
    );
    clickEnqueue();

    await waitFor(() => expect(screen.getByText(/已投递/)).toBeTruthy());
  });

  it("shows a readable error and stays usable when the enqueue is rejected", async () => {
    const pickFile = vi
      .fn<() => Promise<string | null>>()
      .mockResolvedValueOnce("/p/a.spec.md")
      .mockResolvedValueOnce("/p/a.plan.md");
    const enqueue = vi
      .fn<() => Promise<void>>()
      .mockRejectedValue({ message: "spec file does not exist: /p/a.spec.md" });

    render(
      <SuperpowersKanbanEnqueue pickFile={pickFile} enqueue={enqueue} prequeue={noopPrequeue} />,
    );
    clickEnqueue();

    await waitFor(() =>
      expect(screen.getByText("spec file does not exist: /p/a.spec.md")).toBeTruthy(),
    );
    // 失败不关菜单、也不禁用按钮：一对选错的路径是能改的。
    const item = screen.getByRole("menuitem", { name: "加入看板" }) as HTMLButtonElement;
    expect(item.disabled).toBe(false);
  });

  it("ignores a second click while the first is still in flight", async () => {
    let release: (() => void) | undefined;
    const pickFile = vi
      .fn<() => Promise<string | null>>()
      .mockResolvedValueOnce("/p/a.spec.md")
      .mockResolvedValueOnce("/p/a.plan.md");
    const enqueue = vi.fn<() => Promise<void>>(
      () =>
        new Promise<void>((resolve) => {
          release = () => resolve();
        }),
    );

    render(
      <SuperpowersKanbanEnqueue pickFile={pickFile} enqueue={enqueue} prequeue={noopPrequeue} />,
    );
    clickEnqueue();
    clickEnqueue();

    await waitFor(() => expect(enqueue).toHaveBeenCalledTimes(1));
    release?.();
    await waitFor(() => expect(screen.getByText(/已投递/)).toBeTruthy());
    expect(enqueue).toHaveBeenCalledTimes(1);
  });

  it("delivers a pre-queued card from the title input", async () => {
    const calls: string[] = [];
    render(
      <SuperpowersKanbanEnqueue
        pickFile={async () => null}
        enqueue={async () => {}}
        prequeue={async (title) => {
          calls.push(title);
        }}
      />,
    );
    openMenu();
    fireEvent.change(screen.getByLabelText("预排队标题"), { target: { value: "my idea" } });
    fireEvent.click(screen.getByRole("button", { name: "预排队" }));
    expect(calls).toEqual(["my idea"]);
    expect(await screen.findByText("已投递，等待插件校验")).toBeTruthy();
  });

  it("ignores an empty pre-queued title", async () => {
    const calls: string[] = [];
    render(
      <SuperpowersKanbanEnqueue
        pickFile={async () => null}
        enqueue={async () => {}}
        prequeue={async (title) => {
          calls.push(title);
        }}
      />,
    );
    openMenu();
    fireEvent.click(screen.getByRole("button", { name: "预排队" }));
    expect(calls).toEqual([]);
  });
});

describe("SuperpowersKanbanEnqueue menu", () => {
  const props = {
    pickFile: async () => null,
    enqueue: async () => {},
    prequeue: async () => {},
  };

  it("keeps both entries hidden until the plus is clicked", () => {
    render(<SuperpowersKanbanEnqueue {...props} />);
    // 平时只有一个「＋」：入队是低频动作，不该占掉看板首屏。
    expect(screen.queryByRole("menuitem", { name: "加入看板" })).toBeNull();
    expect(screen.queryByLabelText("预排队标题")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "添加到看板" }));
    expect(screen.getByRole("menuitem", { name: "加入看板" })).toBeTruthy();
    expect(screen.getByLabelText("预排队标题")).toBeTruthy();
  });

  it("closes on Escape and gives focus back to the plus", () => {
    render(<SuperpowersKanbanEnqueue {...props} />);
    const trigger = screen.getByRole("button", { name: "添加到看板" });
    fireEvent.click(trigger);
    fireEvent.keyDown(document, { key: "Escape" });
    expect(screen.queryByRole("menuitem", { name: "加入看板" })).toBeNull();
    expect(document.activeElement).toBe(trigger);
  });
});
