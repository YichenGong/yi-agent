/**
 * 受管后台进程详情:状态摘要 + stdout/stderr 分栏 + 追尾 + kill。
 *
 * 输出由调用方增量拉取后传进来(本组件不轮询),这样"只在详情打开时拉"这条
 * 约束由唯一的数据所有者(App)负责,组件保持可独立测试。
 */
import { useEffect, useRef, useState } from "react";
import type { ProcessInfo } from "../lib/protocol";
import { isActive, statusLabel } from "../lib/processes";

/** 一个输出栏:内容追加时若用户仍在底部则跟随,上滚即暂停。 */
function OutputPane({
  title,
  text,
  testId,
  onFollowChange,
}: {
  title: string;
  text: string;
  testId: string;
  onFollowChange: (following: boolean) => void;
}) {
  const ref = useRef<HTMLPreElement | null>(null);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 8;
    if (atBottom) el.scrollTop = el.scrollHeight;
  }, [text]);

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="px-2 py-1 text-xs text-fg-subtle">{title}</div>
      <pre
        ref={ref}
        data-testid={testId}
        className="min-h-0 flex-1 overflow-auto whitespace-pre-wrap px-2 pb-2 font-mono text-xs text-fg-muted"
        onScroll={() => {
          const el = ref.current;
          if (!el) return;
          onFollowChange(el.scrollHeight - el.scrollTop - el.clientHeight < 8);
        }}
      >
        {text}
      </pre>
    </div>
  );
}

export function ProcessDetail({
  info,
  output,
  onClose,
  onKill,
}: {
  info: ProcessInfo;
  output: { stdout: string; stderr: string; truncated: boolean } | null;
  onClose: () => void;
  onKill: () => Promise<void>;
}) {
  const [confirming, setConfirming] = useState(false);
  const [paused, setPaused] = useState(false);
  const submitted = useRef(false);
  // 终态即「非活跃」：exited / killed / failed_to_start 都不再给 kill 入口。
  const terminal = !isActive(info.status);

  useEffect(() => {
    if (!confirming) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setConfirming(false);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [confirming]);

  return (
    <section
      aria-label={`进程详情 ${info.process_id}`}
      className="relative flex min-h-0 flex-1 flex-col border-t border-line bg-surface"
    >
      <div className="flex items-center justify-between px-3 py-2">
        <div className="flex min-w-0 items-center gap-2">
          <span className="text-xs text-fg-muted">[{statusLabel(info.status)}]</span>
          <span className="truncate text-sm text-fg">{info.name ?? info.process_id}</span>
          <span className="text-xs text-fg-subtle">
            {info.pid !== null ? `pid ${info.pid}` : "pid -"}
          </span>
          {terminal && info.exit_code !== null && (
            <span className="text-xs text-fg-subtle">退出码 {info.exit_code}</span>
          )}
        </div>
        <div className="flex items-center gap-2">
          {paused && <span className="text-xs text-amber-400">已暂停追尾</span>}
          <button
            type="button"
            className="text-xs text-fg-muted hover:text-fg"
            onClick={onClose}
            aria-label="关闭进程详情"
          >
            关闭
          </button>
        </div>
      </div>
      <div className="px-3 pb-2 font-mono text-xs text-fg-subtle">{info.command}</div>
      {output?.truncated && <div className="px-3 pb-2 text-xs text-amber-400">较早输出已滚出</div>}
      <div className="flex min-h-0 flex-1 flex-col">
        <OutputPane
          title="stdout"
          text={output?.stdout || "(空)"}
          testId="process-stdout"
          onFollowChange={(f) => setPaused(!f)}
        />
        <OutputPane
          title="stderr"
          text={output?.stderr || "(空)"}
          testId="process-stderr"
          onFollowChange={() => {}}
        />
      </div>
      {!terminal && (
        <div className="flex items-center justify-end gap-2 border-t border-line px-3 py-2">
          <button
            type="button"
            className="rounded border border-red-800 px-2 py-1 text-xs text-red-300 hover:bg-red-950"
            onClick={() => {
              submitted.current = false;
              setConfirming(true);
            }}
          >
            终止进程
          </button>
        </div>
      )}
      {confirming && (
        <div
          role="dialog"
          aria-modal="true"
          aria-label="确认终止进程"
          className="absolute inset-0 z-40 flex items-center justify-center bg-black/60"
        >
          <div className="w-80 rounded border border-line-strong bg-panel p-4">
            <p className="text-sm text-fg">终止 {info.name ?? info.process_id}?</p>
            <div className="mt-3 flex justify-end gap-2">
              <button
                type="button"
                className="rounded px-2 py-1 text-xs text-fg-muted hover:text-fg"
                onClick={() => setConfirming(false)}
              >
                取消
              </button>
              <button
                type="button"
                className="rounded bg-red-800 px-2 py-1 text-xs text-white hover:bg-red-700"
                onClick={async () => {
                  if (submitted.current) return;
                  submitted.current = true;
                  await onKill();
                  setConfirming(false);
                }}
              >
                确认终止
              </button>
            </div>
          </div>
        </div>
      )}
    </section>
  );
}