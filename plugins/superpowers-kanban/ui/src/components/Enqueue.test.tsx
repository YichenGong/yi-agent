import { describe, expect, it, vi } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Enqueue } from "./Enqueue";

const noopPrequeue = async () => {};

type User = ReturnType<typeof userEvent.setup>;

/** The entry lives behind the "＋" menu: open it and type both paths. */
async function fillPaths(
  user: User,
  spec = "/p/a.spec.md",
  plan = "/p/a.plan.md",
): Promise<void> {
  await user.click(screen.getByRole("button", { name: "添加到看板" }));
  await user.type(screen.getByLabelText("spec 路径"), spec);
  await user.type(screen.getByLabelText("plan 路径"), plan);
}

const submit = (user: User) =>
  user.click(screen.getByRole("button", { name: "加入看板" }));

describe("Enqueue", () => {
  it("keeps both entries hidden until the plus is clicked", async () => {
    const user = userEvent.setup();
    render(<Enqueue enqueue={async () => {}} prequeue={noopPrequeue} />);
    expect(screen.queryByRole("button", { name: "加入看板" })).toBeNull();
    await user.click(screen.getByRole("button", { name: "添加到看板" }));
    expect(screen.getByRole("button", { name: "加入看板" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "预排队" })).toBeTruthy();
  });

  it("enqueues once with the two typed paths", async () => {
    const user = userEvent.setup();
    const enqueue = vi.fn<() => Promise<void>>().mockResolvedValue(undefined);
    render(<Enqueue enqueue={enqueue} prequeue={noopPrequeue} />);
    await fillPaths(user);
    await submit(user);
    await waitFor(() => expect(enqueue).toHaveBeenCalledTimes(1));
    expect(enqueue).toHaveBeenCalledWith("/p/a.spec.md", "/p/a.plan.md");
  });

  it("trims the paths before sending them", async () => {
    const user = userEvent.setup();
    const enqueue = vi.fn<() => Promise<void>>().mockResolvedValue(undefined);
    render(<Enqueue enqueue={enqueue} prequeue={noopPrequeue} />);
    await fillPaths(user, "  /p/a.spec.md  ", "  /p/a.plan.md  ");
    await submit(user);
    await waitFor(() => expect(enqueue).toHaveBeenCalledWith("/p/a.spec.md", "/p/a.plan.md"));
  });

  it("does not send when either path is missing", async () => {
    // A half-filled pair is not a card. The backend would reject it, but asking
    // it to reject is a round trip that tells the user nothing new.
    const user = userEvent.setup();
    const enqueue = vi.fn(async () => {});
    render(<Enqueue enqueue={enqueue} prequeue={noopPrequeue} />);
    await user.click(screen.getByRole("button", { name: "添加到看板" }));
    await user.type(screen.getByLabelText("spec 路径"), "/p/a.spec.md");
    await submit(user);
    expect(enqueue).not.toHaveBeenCalled();
  });

  it("says the card is delivered and awaits validation, not that it is running", async () => {
    // "Queued" would be a promise the backend has not made yet: it validates on
    // its next tick, and the card can still be rejected.
    const user = userEvent.setup();
    render(<Enqueue enqueue={async () => {}} prequeue={noopPrequeue} />);
    await fillPaths(user);
    await submit(user);
    expect(await screen.findByText(/已投递，等待后端校验/)).toBeTruthy();
  });

  it("shows a readable error and stays usable when the enqueue is rejected", async () => {
    const user = userEvent.setup();
    render(
      <Enqueue
        enqueue={async () => {
          throw { kind: "Conflict", detail: "卡片已存在" };
        }}
        prequeue={noopPrequeue}
      />,
    );
    await fillPaths(user);
    await submit(user);
    expect(await screen.findByRole("alert")).toHaveTextContent("卡片已存在");
    // A failed delivery must not be reported as delivered.
    expect(screen.queryByText(/已投递/)).toBeNull();
    // The trigger must still work: a failed delivery is not a broken panel.
    expect(screen.getByRole("button", { name: "添加到看板" })).not.toBeDisabled();
  });

  it("reports an unknown write outcome instead of pretending it failed", async () => {
    // A write whose fate is unknown may already have landed; telling the user
    // it failed invites a duplicate delivery.
    const user = userEvent.setup();
    render(
      <Enqueue
        enqueue={async () => {
          throw { kind: "ResultUnknown", detail: "no answer" };
        }}
        prequeue={noopPrequeue}
      />,
    );
    await fillPaths(user);
    await submit(user);
    expect(await screen.findByRole("alert")).toHaveTextContent(/结果未知/);
    expect(screen.queryByText(/已投递/)).toBeNull();
  });

  it("ignores a second click while the first is still in flight", async () => {
    const user = userEvent.setup();
    let release: () => void = () => {};
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    const enqueue = vi.fn(async () => {
      await gate;
    });
    render(<Enqueue enqueue={enqueue} prequeue={noopPrequeue} />);
    await fillPaths(user);
    await submit(user);
    await waitFor(() => expect(enqueue).toHaveBeenCalledTimes(1));
    // The submit button is disabled while the first write is parked, so the
    // second click is a no-op rather than a duplicate delivery.
    expect(screen.getByRole("button", { name: "加入看板" })).toBeDisabled();
    await user.click(screen.getByRole("button", { name: "加入看板" }));
    release();
    await waitFor(() => expect(enqueue).toHaveBeenCalledTimes(1));
  });

  it("delivers a pre-queued card from the title input", async () => {
    const user = userEvent.setup();
    const prequeue = vi.fn(async () => {});
    render(<Enqueue enqueue={async () => {}} prequeue={prequeue} />);
    await user.click(screen.getByRole("button", { name: "添加到看板" }));
    await user.type(screen.getByLabelText("预排队标题"), "  一个想法  ");
    await user.click(screen.getByRole("button", { name: "预排队" }));
    await waitFor(() => expect(prequeue).toHaveBeenCalledWith("一个想法"));
  });

  it("submits a pre-queued title on Enter", async () => {
    const user = userEvent.setup();
    const prequeue = vi.fn(async () => {});
    render(<Enqueue enqueue={async () => {}} prequeue={prequeue} />);
    await user.click(screen.getByRole("button", { name: "添加到看板" }));
    await user.type(screen.getByLabelText("预排队标题"), "按回车提交{Enter}");
    await waitFor(() => expect(prequeue).toHaveBeenCalledWith("按回车提交"));
  });

  it("ignores an empty pre-queued title", async () => {
    const user = userEvent.setup();
    const prequeue = vi.fn(async () => {});
    render(<Enqueue enqueue={async () => {}} prequeue={prequeue} />);
    await user.click(screen.getByRole("button", { name: "添加到看板" }));
    await user.type(screen.getByLabelText("预排队标题"), "   ");
    await user.click(screen.getByRole("button", { name: "预排队" }));
    expect(prequeue).not.toHaveBeenCalled();
  });

  it("returns focus to the trigger when the menu closes", async () => {
    // The menu opens from the keyboard; closing must not drop focus on <body>.
    const user = userEvent.setup();
    render(<Enqueue enqueue={async () => {}} prequeue={noopPrequeue} />);
    const trigger = screen.getByRole("button", { name: "添加到看板" });
    await user.click(trigger);
    expect(screen.getByLabelText("spec 路径")).toBeTruthy();
    await user.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByLabelText("spec 路径")).toBeNull());
    expect(document.activeElement).toBe(trigger);
  });
});
