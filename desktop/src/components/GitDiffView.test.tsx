/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, fireEvent, cleanup, screen } from "@testing-library/react";
import { GitDiffView } from "./GitDiffView";
import type { ThreadDiffResult } from "../lib/protocol";
import type { ViewFrame } from "../lib/viewFrame";

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
    // 收紧成全局横幅自己的文案：此时 n.txt 也没有正文，逐文件标注同样带「截断」二字，
    // 泛化的 /截断/ 会命中两个元素并让 getByText 直接抛错。
    expect(screen.getByText(/diff 过长已截断/)).toBeTruthy();
  });
});

/** 十个字段与 `lib/viewFrame.ts` 的 `ViewFrame` 逐字对齐。 */
const frame = (over: Partial<ViewFrame> = {}): ViewFrame => ({
  command: null,
  asKind: "diff",
  exitCode: 0,
  stdout: "",
  stderr: "",
  truncated: false,
  timedOut: false,
  note: null,
  base: null,
  baseKind: "",
  ...over,
});

describe("GitDiffView 模型帧", () => {
  it("把命令与退出码显示在头部", () => {
    render(
      <GitDiffView
        {...baseProps}
        diff={diff}
        frame={frame({ command: "git log --oneline -1", asKind: "text", stdout: "abc1234 x\n" })}
      />,
    );
    expect(screen.getByText(/git log --oneline -1/)).toBeTruthy();
    expect(screen.getByText(/退出码\s*0/)).toBeTruthy();
  });

  it("log 原语渲染成提交列表", () => {
    render(
      <GitDiffView
        {...baseProps}
        diff={diff}
        frame={frame({
          command: "git log --oneline -2",
          asKind: "log",
          stdout: "abc1234 fix a\ndef5678 add b\n",
        })}
      />,
    );
    expect(screen.getByText("fix a")).toBeTruthy();
    expect(screen.getByText("add b")).toBeTruthy();
  });

  it("汇总行给出增删统计（D4）", () => {
    render(<GitDiffView {...baseProps} diff={diff} />);
    expect(screen.getByText(/\+2/)).toBeTruthy();
    // 简报写的是 /−1|-\s*1/：ASCII 那半会同时命中每个文件行的 "-1"，
    // getByText 会因「找到多个元素」直接报错；收紧成真正的 U+2212 减号后只命中汇总行。
    expect(screen.getByText(/−\s*1/)).toBeTruthy();
  });

  it("session 基准显示为「本次会话」而不是原始 ref（D4）", () => {
    render(
      <GitDiffView {...baseProps} diff={{ ...diff, base: "deadbeef", baseKind: "session" }} />,
    );
    expect(screen.getByText(/本次会话/)).toBeTruthy();
    expect(screen.queryByText(/deadbeef/)).toBeNull();
  });

  it("截断且无正文的文件被明确标注，而不是空白（D1）", () => {
    const onLoadFile = vi.fn();
    render(
      <GitDiffView
        {...baseProps}
        diff={{ ...diff, truncated: true, files: [diff.files[0]], unifiedDiff: "" }}
        onLoadFile={onLoadFile}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /a\.txt/ }));
    fireEvent.click(screen.getByRole("button", { name: /正文已截断/ }));
    expect(screen.getByText(/正文已截断|未加载/)).toBeTruthy();
    expect(onLoadFile).toHaveBeenCalledWith("a.txt");
  });

  it("结构化快照为空（0 files / 0 commits）但有帧时，帧仍然渲染", () => {
    // 「没有改动」空态不能盖掉帧：模型可能就是让我们看 `git log`（与结构化 diff 无关）。
    render(
      <GitDiffView
        {...baseProps}
        diff={{ ...diff, files: [], commits: [], unifiedDiff: "" }}
        frame={frame({ command: "git log --oneline -1", asKind: "log", stdout: "abc1234 fix a\n" })}
      />,
    );
    expect(screen.getByText("fix a")).toBeTruthy();
    expect(screen.queryByText(/没有改动/)).toBeNull();
  });

  it("空壳帧（command 为 undefined、stdout 为空）不顶掉结构化正文", () => {
    // `{command: undefined}` 是训练里见过的坏形状：`!== null` 会放行它，一个空壳帧
    // 会把整份结构化正文换成空白。判据收严成「非空字符串」后它必须退回旧路径。
    render(
      <GitDiffView
        {...baseProps}
        diff={diff}
        frame={{ ...frame(), command: undefined as unknown as string | null, stdout: "" }}
      />,
    );
    // 旧路径仍可见（文件名与提交都在），也没有出现空白的帧正文。
    expect(screen.getByText("a.txt")).toBeTruthy();
    expect(screen.getByText("add feature")).toBeTruthy();
  });
});
