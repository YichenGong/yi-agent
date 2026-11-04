import { describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { BoardCard } from "../lib/cards";
import { Card } from "./Card";

function card(extra: Partial<BoardCard> = {}): BoardCard {
  return {
    id: "c1",
    state: "needs_you",
    progress: null,
    detail: "",
    threadId: null,
    originThreadId: null,
    ...extra,
  };
}

describe("Card", () => {
  it("shows the title, the shortened id and the state", () => {
    render(<Card card={card({ id: "card-1", state: "running", title: "do the thing" })} />);
    expect(screen.getByText("do the thing")).toBeTruthy();
    expect(screen.getByText("running")).toBeTruthy();
    expect(screen.getByText("card-1")).toBeTruthy();
  });

  it("falls back to the id when a card has no title", () => {
    // The id also appears in the shortened-id line, so scope the assertion to
    // the title element rather than matching loose text.
    const { container } = render(<Card card={card({ id: "only-an-id", title: undefined })} />);
    expect(container.querySelector(".kanban-card-title")?.textContent).toBe("only-an-id");
  });

  it("marks a card that needs the user", () => {
    render(<Card card={card({ state: "needs_you" })} />);
    expect(screen.getByLabelText("需你处理")).toBeTruthy();
  });

  it("does not mark a card that does not need the user", () => {
    render(<Card card={card({ state: "running" })} />);
    expect(screen.queryByLabelText("需你处理")).toBeNull();
  });

  it("shows a merge card's source to base, since it has no session to click", () => {
    // A merge card carries no thread id, so the card is the user's only landing
    // point: without `source → base` it is just an id and a red mark.
    render(
      <Card
        card={card({
          kind: "merge",
          source: "kanban/a",
          base: "main",
          detail: "kanban/a → main",
        })}
      />,
    );
    expect(screen.getByText("kanban/a → main")).toBeTruthy();
  });

  it("does not show the merge line on an implementation card", () => {
    render(<Card card={card({ kind: "implementation", detail: "/work/tree" })} />);
    expect(screen.queryByText("/work/tree")).toBeNull();
  });

  it("marks a card that holds the merge slot", () => {
    // Merging is a session turn, so the card is still `running`: without a badge
    // there is no way to tell it apart from "writing code".
    render(<Card card={card({ state: "running", merging: true })} />);
    expect(screen.getByLabelText("正在合并")).toBeTruthy();
  });

  it("does not mark a card that is only queued for a merge slot", () => {
    render(<Card card={card({ state: "merge_queued", merging: false })} />);
    expect(screen.queryByLabelText("正在合并")).toBeNull();
    expect(screen.getByText("merge_queued")).toBeTruthy();
  });

  it("marks a stale card and says when it was last alive", () => {
    // The session is gone but the card still says running. The board must not
    // rewrite the state; it only gives the human a signal they can act on.
    render(
      <Card
        card={card({
          state: "running",
          stale: true,
          lastAliveAt: "2026-10-22T10:00:00+08:00",
        })}
      />,
    );
    expect(screen.getByLabelText("会话已不在")).toBeTruthy();
    expect(screen.getByText(/最后活动/)).toBeTruthy();
  });

  it("does not mark a live running card as stale", () => {
    render(<Card card={card({ state: "running", stale: false })} />);
    expect(screen.queryByLabelText("会话已不在")).toBeNull();
  });
});

describe("Card session link", () => {
  it("shows the discussion session while the card is only pre-queued", async () => {
    const onOpenThread = vi.fn();
    const user = userEvent.setup();
    render(
      <Card
        card={card({
          id: "idea",
          state: "pre_queued",
          originThreadId: "thread-abc",
          threadId: null,
        })}
        onOpenThread={onOpenThread}
      />,
    );
    await user.click(screen.getByRole("button", { name: "thread-abc" }));
    expect(onOpenThread).toHaveBeenCalledWith("thread-abc");
  });

  it("prefers the execution session once the card is running", () => {
    render(
      <Card
        card={card({
          id: "c1",
          state: "running",
          threadId: "thread-exec",
          originThreadId: "thread-abc",
        })}
      />,
    );
    expect(screen.getByRole("button", { name: "thread-exec" })).toBeTruthy();
    // One link per card: showing both would be two answers to "where is this".
    expect(screen.queryByRole("button", { name: "thread-abc" })).toBeNull();
  });

  it("renders no session link when neither session is known", () => {
    render(
      <Card card={card({ id: "idea", state: "pre_queued", threadId: null, originThreadId: null })} />,
    );
    expect(screen.queryByRole("button", { name: /thread-/ })).toBeNull();
  });

  it("opens a merge card's session when it has one", () => {
    const onOpenThread = vi.fn();
    render(
      <Card
        card={card({ kind: "merge", detail: "kanban/a → main", threadId: "thread-m" })}
        onOpenThread={onOpenThread}
      />,
    );
    expect(screen.getByRole("button", { name: "thread-m" })).toBeTruthy();
  });
});

describe("Card promotion", () => {
  it("offers promotion only for a pre-queued card", async () => {
    const user = userEvent.setup();
    render(
      <Card card={card({ id: "idea", state: "pre_queued" })} onPromote={async () => {}} />,
    );
    expect(screen.getByRole("button", { name: "补 spec/plan 提升" })).toBeTruthy();
    // Queued cards get no promotion entry at all.
    render(<Card card={card({ id: "q", state: "queued" })} onPromote={async () => {}} />);
    expect(screen.getAllByRole("button", { name: "补 spec/plan 提升" })).toHaveLength(1);
    await user.click(screen.getByRole("button", { name: "补 spec/plan 提升" }));
  });

  it("sends both typed paths with the card id when promoted", async () => {
    const user = userEvent.setup();
    const onPromote = vi.fn(async () => {});
    render(<Card card={card({ id: "idea", state: "pre_queued" })} onPromote={onPromote} />);
    await user.click(screen.getByRole("button", { name: "补 spec/plan 提升" }));
    await user.type(screen.getByLabelText("spec 路径"), "/p/a.spec.md");
    await user.type(screen.getByLabelText("plan 路径"), "/p/a.plan.md");
    await user.click(screen.getByRole("button", { name: "提升" }));
    expect(onPromote).toHaveBeenCalledWith("idea", "/p/a.spec.md", "/p/a.plan.md");
  });

  it("keeps the form open when the promote is rejected", async () => {
    // A failure is usually a typo in one path; closing the form would make the
    // user retype both.
    const user = userEvent.setup();
    render(
      <Card
        card={card({ id: "idea", state: "pre_queued" })}
        onPromote={async () => {
          throw new Error("spec 不存在");
        }}
      />,
    );
    await user.click(screen.getByRole("button", { name: "补 spec/plan 提升" }));
    await user.type(screen.getByLabelText("spec 路径"), "/nope/a.spec.md");
    await user.type(screen.getByLabelText("plan 路径"), "/p/a.plan.md");
    await user.click(screen.getByRole("button", { name: "提升" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("spec 不存在");
    expect(screen.getByLabelText("spec 路径")).toHaveValue("/nope/a.spec.md");
  });

  it("offers no promotion for a queued card", () => {
    render(<Card card={card({ state: "queued" })} onPromote={async () => {}} />);
    expect(screen.queryByRole("button", { name: "补 spec/plan 提升" })).toBeNull();
  });

  it("shortens a long id in the middle so both ends stay recognisable", () => {
    const id = "2026-10-03-a-very-long-card-identifier-tail";
    const { container } = render(<Card card={card({ id, title: "t" })} />);
    const shown = container.querySelector(".kanban-mono")?.textContent ?? "";
    expect(shown).toContain("…");
    expect(shown.startsWith("2026-10-03-a-v")).toBe(true);
    expect(shown.endsWith("ifier-tail")).toBe(true);
    // The full id stays available as a tooltip.
    expect(container.querySelector(`[title="${id}"]`)).not.toBeNull();
  });
});
