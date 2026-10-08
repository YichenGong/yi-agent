/**
 * Git Diff 视图：接近 GitHub 的「Commits + Files changed」。
 *
 * 只消费结构化数据，不自己解析 diff 文本（解析在 `lib/gitDiff.ts`，有独立单测）。
 * 默认只展开前若干个文件：长 diff 一次渲染成百上千行会拖慢整块面板。
 */
import { memo, useState } from "react";
import type { CommitInfo, FileStat, ThreadDiffResult } from "../lib/protocol";
import { parseUnifiedDiff, type FileDiff } from "../lib/gitDiff";

const DEFAULT_EXPANDED = 3;

/** 文件状态字母的配色，复用 diff 的增删语义色，切主题时一起换。 */
function statusBadge(status: string): string {
  switch (status) {
    case "A":
      return "text-diff-add-fg";
    case "D":
      return "text-diff-del-fg";
    case "R":
      return "text-amber-400";
    default:
      return "text-fg-muted";
  }
}

function relativeTime(ts: number): string {
  const secs = Math.max(0, Math.floor(Date.now() / 1000) - ts);
  if (secs < 60) return "刚刚";
  if (secs < 3600) return `${Math.floor(secs / 60)} 分钟前`;
  if (secs < 86400) return `${Math.floor(secs / 3600)} 小时前`;
  return `${Math.floor(secs / 86400)} 天前`;
}

const FileBlock = memo(function FileBlock({ file, open }: { file: FileDiff; open: boolean }) {
  return (
    <div className="mt-1 overflow-hidden">
      {open &&
        file.hunks.map((hunk, hi) => (
          <div key={hi} className="font-mono text-xs">
            {/*
             * hunk 头用 `raised` 底 + `fg-subtle` 字：`bg-panel` 与行底色太接近，
             * 头尾分不开；`fg-subtle` 比 `fg-faint` 亮一档，在两种主题下都读得出。
             */}
            <div className="bg-raised px-2 py-0.5 text-fg-subtle">{hunk.header}</div>
            {hunk.lines.map((line, li) => (
              <div
                key={li}
                className={
                  line.kind === "add"
                    ? "bg-diff-add-bg text-diff-add-fg"
                    : line.kind === "del"
                      ? "bg-diff-del-bg text-diff-del-fg"
                      : "text-fg-subtle"
                }
              >
                {/* 行号：`fg-subtle` 而非 `fg-faint`——后者在深底上几乎融进背景。 */}
                <span className="inline-block w-10 select-none pr-2 text-right text-fg-subtle/70">
                  {line.oldNo ?? ""}
                </span>
                <span className="inline-block w-10 select-none pr-2 text-right text-fg-subtle/70">
                  {line.newNo ?? ""}
                </span>
                <span>{line.kind === "add" ? "+" : line.kind === "del" ? "-" : " "}</span>
                <span className="whitespace-pre-wrap">{line.text}</span>
              </div>
            ))}
          </div>
        ))}
    </div>
  );
});

export function GitDiffView({
  diff,
  loading,
  error,
  activeCommit,
  onRefresh,
  onOpenCommit,
  onCloseCommit,
  note,
}: {
  diff: ThreadDiffResult | null;
  loading: boolean;
  error: string | null;
  activeCommit: { sha: string; text: string } | null;
  onRefresh: () => void;
  onOpenCommit: (sha: string) => void;
  onCloseCommit: () => void;
  note: string | null;
}) {
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});
  const parsed = diff ? parseUnifiedDiff(diff.unifiedDiff) : [];

  if (activeCommit) {
    const commitFiles = parseUnifiedDiff(activeCommit.text);
    return (
      <div className="flex min-h-0 flex-1 flex-col px-3 py-2">
        <div className="flex items-center gap-2">
          <button type="button" className="text-xs text-sky-300 hover:text-sky-200" onClick={onCloseCommit}>
            ← 返回全部改动
          </button>
          <span className="font-mono text-xs text-fg-muted">{activeCommit.sha.slice(0, 8)}</span>
        </div>
        <div className="min-h-0 flex-1 overflow-y-auto">
          {commitFiles.map((f) => (
            <FileBlock key={f.path} file={f} open />
          ))}
        </div>
      </div>
    );
  }

  if (error) {
    return <p className="px-3 py-2 text-sm text-red-300">{error}</p>;
  }
  if (loading && !diff) {
    return <p className="px-3 py-2 text-sm text-fg-subtle">正在计算 diff…</p>;
  }
  if (!diff) {
    return <p className="px-3 py-2 text-sm text-fg-subtle">尚无 diff 数据</p>;
  }
  const empty = diff.files.length === 0 && diff.commits.length === 0;
  if (empty) {
    return (
      <div className="px-3 py-2 text-sm text-fg-muted">
        <p>没有改动</p>
        <p className="mt-1 text-xs text-fg-faint">
          基准：{diff.base ?? "（无默认分支，仅看工作区）"}
        </p>
      </div>
    );
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex items-center justify-between px-3 py-2">
        <span className="truncate text-xs text-fg-muted">
          对比 {diff.base ?? "工作区"}
          {diff.mergeBase && ` · merge-base ${diff.mergeBase.slice(0, 7)}`}
          {` · ${diff.commits.length} commits · ${diff.files.length} files`}
        </span>
        <button type="button" className="text-xs text-fg-subtle hover:text-fg-muted" onClick={onRefresh}>
          刷新
        </button>
      </div>
      {note && <p className="px-3 pb-1 text-xs text-sky-300">{note}</p>}
      {diff.truncated && (
        <p className="px-3 pb-1 text-xs text-amber-300">diff 过长已截断，仅显示前一部分</p>
      )}

      <div className="min-h-0 flex-1 overflow-y-auto px-3 pb-3">
        {diff.commits.length > 0 && (
          <div className="mb-2">
            <p className="text-xs text-fg-subtle">Commits</p>
            <ul>
              {diff.commits.map((c: CommitInfo) => (
                <li key={c.sha}>
                  <button
                    type="button"
                    className="flex w-full items-center gap-2 rounded px-1 py-0.5 text-left hover:bg-raised/50"
                    onClick={() => onOpenCommit(c.sha)}
                  >
                    <span className="font-mono text-xs text-fg-faint">{c.short}</span>
                    <span className="min-w-0 flex-1 truncate text-sm text-fg">{c.subject}</span>
                    <span className="text-xs text-fg-faint">{relativeTime(c.timestamp)}</span>
                  </button>
                </li>
              ))}
            </ul>
          </div>
        )}

        <p className="text-xs text-fg-subtle">Files changed</p>
        <ul>
          {diff.files.map((f: FileStat, i: number) => {
            const open = expanded[f.path] ?? i < DEFAULT_EXPANDED;
            return (
              <li key={f.path} className="border-b border-line/50">
                <button
                  type="button"
                  className="flex w-full items-center gap-2 px-1 py-1 text-left"
                  aria-expanded={open}
                  onClick={() => setExpanded((e) => ({ ...e, [f.path]: !open }))}
                >
                  <span className={`w-4 text-center font-mono text-xs ${statusBadge(f.status)}`}>
                    {f.status}
                  </span>
                  <span className="min-w-0 flex-1 truncate font-mono text-xs text-fg">{f.path}</span>
                  <span className="text-xs text-diff-add-fg">+{f.additions}</span>
                  <span className="text-xs text-diff-del-fg">-{f.deletions}</span>
                </button>
                {f.binary ? (
                  <p className="px-1 pb-1 text-xs text-fg-faint">二进制文件，不显示内容</p>
                ) : (
                  <FileBlock
                    file={parsed.find((p) => p.path === f.path) ?? { ...f, hunks: [] }}
                    open={open}
                  />
                )}
              </li>
            );
          })}
        </ul>
      </div>
    </div>
  );
}
