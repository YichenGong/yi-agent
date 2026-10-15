/**
 * 主对话旁的受管后台进程暂留区。
 *
 * 与「子 agent」并列成 tab、可折叠,不是弹窗:用户要能在读主对话的同时瞥见
 * 后台服务是否还活着。点卡片回调 `onOpen(processId)` 进入详情。
 *
 * `embedded` 时不自带外框(与 `SubagentRail` 同一约定):嵌入右栏 Tab 时,
 * 外框与页眉(tab 条)由容器 `ThreadDetailPanel` 提供。
 */
import { useState } from "react";
import type { ProcessInfo } from "../lib/protocol";
import {
  formatElapsed,
  partitionProcesses,
  processDisplayName,
  statusColor,
  statusLabel,
} from "../lib/processes";

function ProcessCard({
  process,
  selected,
  onOpen,
}: {
  process: ProcessInfo;
  selected: boolean;
  onOpen: (id: string) => void;
}) {
  const name = processDisplayName(process);
  return (
    <button
      type="button"
      aria-label={`查看进程 ${name}`}
      onClick={() => onOpen(process.process_id)}
      aria-current={selected ? "true" : undefined}
      className={`w-full rounded border px-3 py-2 text-left hover:border-line-strong ${
        selected ? "border-sky-600 bg-panel" : "border-line bg-panel"
      }`}
    >
      <div className="flex items-center gap-2">
        <span className={`text-xs ${statusColor(process.status)}`}>
          [{statusLabel(process.status)}]
        </span>
        <span className="text-xs text-fg-subtle">
          {process.pid !== null ? `pid ${process.pid}` : "pid -"}
        </span>
        <span className="text-xs text-fg-subtle">{formatElapsed(process.elapsed_sec)}</span>
      </div>
      <div className="mt-1 truncate text-sm text-fg">{name}</div>
      <div className="mt-1 truncate font-mono text-xs text-fg-subtle">{process.command}</div>
    </button>
  );
}

export function ProcessRail({
  processes,
  selectedProcessId,
  onOpen,
  embedded = false,
}: {
  processes: ProcessInfo[];
  onOpen: (processId: string) => void;
  /** 用户已打开的进程,高亮让暂留区显示选择。 */
  selectedProcessId?: string | null;
  /** 嵌入右栏时不再自带外框（容器已提供）。 */
  embedded?: boolean;
}) {
  const [showFinished, setShowFinished] = useState(false);
  const { active, finished } = partitionProcesses(processes);

  const list = (
    <div
      className="min-h-0 flex-1 overflow-y-auto px-2 pb-3"
      {...(embedded ? { "data-embedded": "" } : {})}
    >
      {processes.length === 0 ? (
        <p className="px-3 py-2 text-sm text-fg-subtle">暂无后台进程</p>
      ) : (
        <>
          {active.length === 0 ? (
            <p className="px-1 py-2 text-sm text-fg-subtle">暂无运行中的后台进程</p>
          ) : (
            <ul className="flex flex-col gap-2">
              {active.map((p) => (
                <li key={p.process_id}>
                  <ProcessCard
                    process={p}
                    selected={selectedProcessId === p.process_id}
                    onOpen={onOpen}
                  />
                </li>
              ))}
            </ul>
          )}
          {finished.length > 0 && (
            <div className="mt-3">
              <button
                type="button"
                aria-expanded={showFinished}
                className="text-xs text-fg-subtle hover:text-fg-muted"
                onClick={() => setShowFinished((v) => !v)}
              >
                已结束 ({finished.length}) {showFinished ? "收起" : "展开"}
              </button>
              {showFinished && (
                <ul className="mt-2 flex flex-col gap-2">
                  {finished.map((p) => (
                    <li key={p.process_id}>
                      <ProcessCard
                        process={p}
                        selected={selectedProcessId === p.process_id}
                        onOpen={onOpen}
                      />
                    </li>
                  ))}
                </ul>
              )}
            </div>
          )}
        </>
      )}
    </div>
  );

  if (embedded) return list;

  return (
    <aside
      aria-label="后台进程"
      className="flex w-72 min-w-0 shrink-0 flex-col border-l border-line bg-raised"
    >
      <div className="flex items-center justify-between px-3 py-2">
        <h2 className="text-xs font-medium uppercase tracking-wide text-fg-muted">
          后台进程{processes.length > 0 ? ` (${processes.length})` : ""}
        </h2>
      </div>
      {list}
    </aside>
  );
}