/** @vitest-environment jsdom */
import { describe, expect, it, vi, afterEach } from "vitest";
import { fireEvent, render, cleanup, screen, waitFor } from "@testing-library/react";
import { SuperpowersKanbanEnqueue } from "./SuperpowersKanbanEnqueue";

afterEach(() => {
  cleanup();
});

const clickEnqueue = () => fireEvent.click(screen.getByRole("button", { name: "加入看板" }));

describe("SuperpowersKanbanEnqueue", () => {
  it("enqueues once with both picked paths", async () => {
    const pickFile = vi
      .fn<() => Promise<string | null>>()
      .mockResolvedValueOnce("/p/a.spec.md")
      .mockResolvedValueOnce("/p/a.plan.md");
    const enqueue = vi.fn<() => Promise<void>>().mockResolvedValue(undefined);

    render(<SuperpowersKanbanEnqueue pickFile={pickFile} enqueue={enqueue} />);
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

    render(<SuperpowersKanbanEnqueue pickFile={pickFile} enqueue={enqueue} />);
    clickEnqueue();

    await waitFor(() => expect(order).toEqual(["pick-1", "pick-2", "enqueue"]));
  });

  it("stops without enqueuing when the spec pick is cancelled", async () => {
    const pickFile = vi.fn<() => Promise<string | null>>().mockResolvedValue(null);
    const enqueue = vi.fn<() => Promise<void>>().mockResolvedValue(undefined);

    render(<SuperpowersKanbanEnqueue pickFile={pickFile} enqueue={enqueue} />);
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

    render(<SuperpowersKanbanEnqueue pickFile={pickFile} enqueue={enqueue} />);
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

    render(<SuperpowersKanbanEnqueue pickFile={pickFile} enqueue={enqueue} />);
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

    render(<SuperpowersKanbanEnqueue pickFile={pickFile} enqueue={enqueue} />);
    clickEnqueue();

    await waitFor(() =>
      expect(screen.getByText("spec file does not exist: /p/a.spec.md")).toBeTruthy(),
    );
    // The button must not be left disabled: a rejected pair is fixable.
    expect(screen.getByRole("button", { name: "加入看板" })).toBeTruthy();
    expect((screen.getByRole("button", { name: "加入看板" }) as HTMLButtonElement).disabled).toBe(
      false,
    );
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

    render(<SuperpowersKanbanEnqueue pickFile={pickFile} enqueue={enqueue} />);
    clickEnqueue();
    clickEnqueue();

    await waitFor(() => expect(enqueue).toHaveBeenCalledTimes(1));
    release?.();
    await waitFor(() => expect(screen.getByText(/已投递/)).toBeTruthy());
    expect(enqueue).toHaveBeenCalledTimes(1);
  });
});
