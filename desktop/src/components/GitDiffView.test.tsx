/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, fireEvent, cleanup, screen } from "@testing-library/react";
import { GitDiffView } from "./GitDiffView";
import type { ThreadDiffResult } from "../lib/protocol";

afterEach(cleanup);

const diff: ThreadDiffResult = {
  base: "origin/main",
  baseKind: "origin-default",
  mergeBase: "abc1234",
  commits: [
    { sha: "c1", short: "c1", subject: "add feature", author: "T", timestamp: 1700000000 },
  ],
  files: [
    { path: "a.txt", status: "M", additions: 1, deletions: 1, binary: false },
    { path: "n.txt", status: "A", additions: 1, deletions: 0, binary: false },
  ],
  unifiedDiff: [
    "diff --git a/a.txt b/a.txt",
    "--- a/a.txt",
    "+++ b/a.txt",
    "@@ -1 +1 @@",
    "-old",
    "+new",
    "",
  ].join("\n"),
  truncated: false,
};

const baseProps = {
  loading: false,
  error: null,
  activeCommit: null,
  onRefresh: () => {},
  onOpenCommit: () => {},
  onCloseCommit: () => {},
  note: null,
};

describe("GitDiffView", () => {
  it("summarises the base, commits and files", () => {
    render(<GitDiffView {...baseProps} diff={diff} />);
    expect(screen.getByText(/origin\/main/)).toBeTruthy();
    expect(screen.getByText("add feature")).toBeTruthy();
    expect(screen.getByText("a.txt")).toBeTruthy();
  });

  it("renders hunks by default and collapses them on click", () => {
    render(<GitDiffView {...baseProps} diff={diff} />);
    // a.txt 是第 0 个文件，落在 DEFAULT_EXPANDED（=3）内，默认应已展开。
    expect(screen.getByText("new")).toBeTruthy();
    expect(screen.getByText("old")).toBeTruthy();
    // 点一下折叠它：hunk 文本随之消失。
    fireEvent.click(screen.getByRole("button", { name: /a\.txt/ }));
    expect(screen.queryByText("new")).toBeNull();
  });

  it("calls onOpenCommit with the clicked sha", () => {
    const onOpenCommit = vi.fn();
    render(<GitDiffView {...baseProps} diff={diff} onOpenCommit={onOpenCommit} />);
    fireEvent.click(screen.getByText("add feature"));
    expect(onOpenCommit).toHaveBeenCalledWith("c1");
  });

  it("shows explicit empty and error states", () => {
    render(<GitDiffView {...baseProps} diff={null} error="not a git repository" />);
    expect(screen.getByText(/not a git repository/)).toBeTruthy();
    render(<GitDiffView {...baseProps} diff={{ ...diff, files: [], commits: [], unifiedDiff: "" }} />);
    expect(screen.getByText(/没有改动/)).toBeTruthy();
  });

  it("flags a truncated diff", () => {
    render(<GitDiffView {...baseProps} diff={{ ...diff, truncated: true }} />);
    expect(screen.getByText(/截断/)).toBeTruthy();
  });
});
