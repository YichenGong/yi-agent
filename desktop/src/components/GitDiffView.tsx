/**
 * Git Diff 视图：接近 GitHub 的「Commits + Files changed」。
 *
 * 只消费结构化数据，不自己解析 diff 文本（解析在 `lib/gitDiff.ts`，有独立单测）。
 * 默认只展开前若干个文件：长 diff 一次渲染成百上千行会拖慢整块面板。
 *
 * 模型驱动：工具 `show_git_diff` 可以把「一条命令的输出」带回来（`frame`）。有帧
 * 且帧有内容时，头部显示命令原文与退出码，正文按 `resolveKind` 分派到三种原语；
 * 无帧时完全走既有的结构化 diff 路径（向后兼容）。
 */
import { memo, useState } from "react";
import type { CommitInfo, FileStat, ThreadDiffResult } from "../lib/protocol";
import { parseUnifiedDiff, type FileDiff } from "../lib/gitDiff";
import { parseLog, resolveKind, type LogEntry, type ViewFrame } from "../lib/viewFrame";

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

/** 汇总行统计：从 `files[]` 求和，不额外调 git（spec §5.8 D4）。 */
function totals(files: FileStat[]): { additions: number; deletions: number } {
  let additions = 0;
  let deletions = 0;
  for (const f of files) {
    additions += f.additions;
    deletions += f.deletions;
  }
  return { additions, deletions };
}

/** 帧「有内容」：命令、stdout、stderr 任一非空。空帧退化成结构化 diff。 */
function frameHasContent(f: ViewFrame): boolean {
  return f.command !== null || f.stdout !== "" || f.stderr !== "";
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

/** `log` 原语：短 sha + 描述。丢弃解析不出的行（装饰、噪声）。 */
function LogBlock({ entries }: { entries: LogEntry[] }) {
  if (entries.length === 0) {
    return <p className="px-3 pb-3 text-xs text-fg-faint">没有可显示的提交</p>;
  }
  return (
    <ul className="px-3 pb-3">
      {entries.map((e, i) => (
        <li key={`${e.sha}-${i}`} className="flex items-center gap-2 py-0.5">
          <span className="font-mono text-xs text-fg-faint">{e.sha.slice(0, 7)}</span>
          <span className="min-w-0 flex-1 truncate text-sm text-fg">{e.subject}</span>
        </li>
      ))}
    </ul>
  );
}

/** `diff` 原语：复用 `parseUnifiedDiff`，无 hunk 的文件显式说明而不是留白。 */
function FrameDiffBody({ text }: { text: string }) {
  const files = parseUnifiedDiff(text);
  if (files.length === 0) {
    return <p className="px-3 pb-3 text-xs text-fg-faint">没有可显示的改动</p>;
  }
  return (
    <div className="px-3 pb-3">
      {files.map((f) => (
        <div key={f.path} className="border-b border-line/50">
          <div className="flex items-center gap-2 px-1 py-1">
            <span className={`w-4 text-center font-mono text-xs ${statusBadge(f.status)}`}>
              {f.status}
            </span>
            <span className="min-w-0 flex-1 truncate font-mono text-xs text-fg">{f.path}</span>
            <span className="text-xs text-diff-add-fg">+{f.additions}</span>
            <span className="text-xs text-diff-del-fg">-{f.deletions}</span>
          </div>
          {f.binary ? (
            <p className="px-1 pb-1 text-xs text-fg-faint">二进制文件，不显示内容</p>
          ) : f.hunks.length === 0 ? (
            <p className="px-1 pb-1 text-xs text-fg-faint">无内容变化</p>
          ) : (
            <FileBlock file={f} open />
          )}
        </div>
      ))}
    </div>
  );
}

/** 正文分派：声明与内容不一致时 `resolveKind` 会退化成 `text`，保底永不失手。 */
function FrameBody({ frame }: { frame: ViewFrame }) {
  const kind = resolveKind(frame);
  if (kind === "log") return <LogBlock entries={parseLog(frame.stdout)} />;
  if (kind === "diff") return <FrameDiffBody text={frame.stdout} />;
  return (
    <pre className="overflow-x-auto whitespace-pre-wrap px-3 pb-3 font-mono text-xs text-fg-subtle">
      {frame.stdout}
    </pre>
  );
}

export function GitDiffView({
  diff,
  loading,
  error,
  activeCommit,
  onRefresh,
  onOpenCommit,
  onCloseCommit,
  note,
  frame,
  onLoadFile,
}: {
  diff: ThreadDiffResult | null;
  loading: boolean;
  error: string | null;
  activeCommit: { sha: string; text: string } | null;
  onRefresh: () => void;
  onOpenCommit: (sha: string) => void;
  onCloseCommit: () => void;
  note: string | null;
  /** 模型给的视图帧（`thread/view/read`）。缺席时走既有结构化 diff 路径。 */
  frame?: ViewFrame | null;
  /** 单文件正文懒加载：文件被截断、正文缺席时点标注回调（spec §5.6 D1）。 */
  onLoadFile?: (path: string) => void;
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
          基准：{diff.baseKind === "session" ? "本次会话" : diff.base ?? "（无默认分支，仅看工作区）"}
        </p>
      </div>
    );
  }

  // 帧存在且有内容时它说了算；否则退回结构化 diff（一字不改的旧路径）。
  const frameBody = frame && frameHasContent(frame) ? frame : null;
  const { additions, deletions } = totals(diff.files);
  const base = frameBody ? frameBody.base : diff.base;
  const baseKind = frameBody ? frameBody.baseKind : diff.baseKind;
  const baseLabel = baseKind === "session" ? "本次会话" : base ?? "工作区";

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex items-center justify-between px-3 py-2">
        <span className="truncate text-xs text-fg-muted">
          对比 {baseLabel}
          {diff.mergeBase && ` · merge-base ${diff.mergeBase.slice(0, 7)}`}
          {` · ${diff.commits.length} commits · ${diff.files.length} files`}
          {` · +${additions} −${deletions}`}
        </span>
        <button type="button" className="text-xs text-fg-subtle hover:text-fg-muted" onClick={onRefresh}>
          刷新
        </button>
      </div>
      {frameBody && (
        <div className="flex items-center gap-2 px-3 pb-1">
          <span className="min-w-0 flex-1 truncate font-mono text-xs text-fg-subtle">
            {frameBody.command ?? "（结构化默认）"}
          </span>
          {frameBody.timedOut && <span className="text-xs text-red-300">命令超时</span>}
          {frameBody.truncated && <span className="text-xs text-amber-300">输出已截断</span>}
          {frameBody.exitCode !== null && (
            <span
              className={`text-xs ${frameBody.exitCode === 0 ? "text-fg-faint" : "text-red-300"}`}
            >
              退出码 {frameBody.exitCode}
            </span>
          )}
        </div>
      )}
      {note && <p className="px-3 pb-1 text-xs text-sky-300">{note}</p>}
      {diff.truncated && (
        <p className="px-3 pb-1 text-xs text-amber-300">diff 过长已截断，仅显示前一部分</p>
      )}

      {frameBody ? (
        <div className="min-h-0 flex-1 overflow-y-auto">
          <FrameBody frame={frameBody} />
        </div>
      ) : (
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
              const body = parsed.find((p) => p.path === f.path);
              // 截断后正文缺席（该 path 不在解析结果里）：给一条能点的标注，
              // 而不是一片读起来像坏了的空白（spec §5.6 D1）。
              const missingBody = diff.truncated && (!body || body.hunks.length === 0);
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
                  ) : missingBody ? (
                    <button
                      type="button"
                      className="px-1 pb-1 text-left text-xs text-amber-300 hover:text-amber-200"
                      onClick={() => onLoadFile?.(f.path)}
                    >
                      正文已截断，点击重新加载
                    </button>
                  ) : (
                    <FileBlock file={body ?? { ...f, hunks: [] }} open={open} />
                  )}
                </li>
              );
            })}
          </ul>
        </div>
      )}
    </div>
  );
}
