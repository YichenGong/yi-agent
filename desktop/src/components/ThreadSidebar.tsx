import { useState } from "react";
import type { ThreadSummary } from "../lib/protocol";

/** Compact relative time, e.g. "3m", "2h", "5d". */
function relativeTime(ms: number): string {
  const diff = Date.now() - ms;
  const sec = Math.floor(diff / 1000);
  if (sec < 60) return "now";
  const min = Math.floor(sec / 60);
  if (min < 60) return `${min}m`;
  const hour = Math.floor(min / 60);
  if (hour < 24) return `${hour}h`;
  const day = Math.floor(hour / 24);
  return `${day}d`;
}

export function ThreadSidebar({
  threads,
  currentId,
  busy,
  onSelect,
  onRename,
  onDelete,
  onNew,
}: {
  threads: ThreadSummary[];
  currentId: string | null;
  busy: boolean;
  onSelect: (id: string) => void;
  onRename: (id: string, title: string) => void;
  onDelete: (id: string) => void;
  onNew: () => void;
}) {
  const [editingId, setEditingId] = useState<string | null>(null);
  const [draft, setDraft] = useState("");

  const commit = (id: string) => {
    const title = draft.trim();
    if (title) onRename(id, title);
    setEditingId(null);
  };

  return (
    <aside className="flex w-64 shrink-0 flex-col border-r border-neutral-800 bg-neutral-900">
      <button
        type="button"
        onClick={onNew}
        disabled={busy}
        className="m-2 rounded-md bg-neutral-800 px-3 py-2 text-left text-sm text-neutral-100 hover:bg-neutral-700 disabled:opacity-50"
      >
        + New thread
      </button>
      <div className="flex-1 overflow-y-auto">
        {threads.map((t) => {
          const active = t.thread_id === currentId;
          return (
            <div
              key={t.thread_id}
              onClick={(e) => {
                if (busy || editingId) return;
                // 双击会先派发两次 click 再派发 dblclick;忽略第二次 click,
                // 避免对同一 thread 触发两次 onSelect(即两次 thread/resume)。
                if (e.detail > 1) return;
                onSelect(t.thread_id);
              }}
              className={`group flex items-center justify-between gap-1 px-3 py-2 text-sm ${
                active ? "bg-neutral-800 text-neutral-100" : "text-neutral-400 hover:bg-neutral-800/50"
              } ${busy ? "cursor-not-allowed opacity-60" : "cursor-pointer"}`}
            >
              {editingId === t.thread_id ? (
                <input
                  autoFocus
                  value={draft}
                  onChange={(e) => setDraft(e.target.value)}
                  onBlur={() => commit(t.thread_id)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") commit(t.thread_id);
                    if (e.key === "Escape") setEditingId(null);
                  }}
                  className="w-full rounded bg-neutral-950 px-1 py-0.5 text-sm text-neutral-100 outline-none"
                />
              ) : (
                <>
                  <div className="min-w-0 flex-1">
                    <div
                      className="truncate"
                      title={t.title ?? t.thread_id}
                      onDoubleClick={() => {
                        if (busy) return;
                        setEditingId(t.thread_id);
                        setDraft(t.title ?? "");
                      }}
                    >
                      {t.title ?? "(untitled)"}
                    </div>
                    <div className="text-xs text-neutral-600">{relativeTime(t.updated_at)}</div>
                  </div>
                  <button
                    type="button"
                    disabled={busy}
                    onClick={(e) => {
                      e.stopPropagation();
                      onDelete(t.thread_id);
                    }}
                    title="Delete"
                    aria-label="Delete thread"
                    className="shrink-0 rounded px-1 text-neutral-500 opacity-0 group-hover:opacity-100 hover:text-red-400 focus-visible:opacity-100 disabled:opacity-50"
                  >
                    ×
                  </button>
                </>
              )}
            </div>
          );
        })}
        {threads.length === 0 && (
          <p className="px-3 py-2 text-xs text-neutral-600">No history yet.</p>
        )}
      </div>
    </aside>
  );
}
