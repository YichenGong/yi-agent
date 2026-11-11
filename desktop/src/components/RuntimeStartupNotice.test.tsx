/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, fireEvent } from "@testing-library/react";
import { RuntimeStartupNotice } from "./RuntimeStartupNotice";
import type { RuntimeStartupIssue } from "../lib/runtimeStartupIssue";

const issue = (project: string, reason: string): RuntimeStartupIssue => ({
  project,
  requestSeq: 1,
  stage: "acquire",
  state: "blocked",
  reason,
});

const issues = (...items: RuntimeStartupIssue[]) =>
  new Map(items.map((item) => [item.project, item]));

afterEach(cleanup);

describe("RuntimeStartupNotice", () => {
  it("renders the fixed reason for the project on screen", () => {
    render(
      <RuntimeStartupNotice
        issues={issues(issue("/p/a", "本机运行环境未能就绪，项目暂时无法运行工具"))}
        project="/p/a"
        local
        onRetry={() => {}}
      />,
    );
    expect(screen.getByRole("alert").textContent).toContain("未能就绪");
  });

  it("shows nothing when the current project is fine", () => {
    const { container } = render(
      <RuntimeStartupNotice
        issues={issues(issue("/p/a", "x"))}
        project="/p/b"
        local
        onRetry={() => {}}
      />,
    );
    expect(container.textContent).toBe("");
  });

  it("shows nothing without an active project", () => {
    const { container } = render(
      <RuntimeStartupNotice
        issues={issues(issue("/p/a", "x"))}
        project={null}
        local
        onRetry={() => {}}
      />,
    );
    expect(container.textContent).toBe("");
  });

  it("offers a retry on the desktop that only re-runs preparation", () => {
    const onRetry = vi.fn();
    render(
      <RuntimeStartupNotice
        issues={issues(issue("/p/a", "x"))}
        project="/p/a"
        local
        onRetry={onRetry}
        // Must not be mistaken for a retry: the turn is the user's next action.
        onOpenProject={vi.fn()}
      />,
    );
    const retry = screen.getByRole("button", { name: "重试" });
    fireEvent.click(retry);
    expect(onRetry).toHaveBeenCalledWith("/p/a");
  });

  it("never shows the local retry to a remote client", () => {
    const onRetry = vi.fn();
    render(
      <RuntimeStartupNotice
        issues={issues(issue("/p/a", "x"))}
        project="/p/a"
        local={false}
        onRetry={onRetry}
        onOpenProject={vi.fn()}
      />,
    );
    expect(screen.queryByRole("button", { name: "重试" })).toBeNull();
    // But the problem must still be visible: silence is what confused people.
    expect(screen.getByRole("alert").textContent).toContain("x");
  });
});
