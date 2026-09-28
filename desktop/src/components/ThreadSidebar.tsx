import { useCallback, useEffect, useRef, useState } from "react";
import type { ThreadSummary, Workspace, WorkspaceGroup } from "../lib/protocol";
import { basename, groupCount } from "../lib/workspaceGroups";
import { clampSidebarWidth, loadSidebarWidth, saveSidebarWidth } from "../lib/sidebarWidth";

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
  groups,
  workspaces,
  currentId,
  busy,
  onSelect,
  onRename,
  onDelete,
  onNew,
  onRemoveWorkspace,
  onBrowse,
}: {
  groups: WorkspaceGroup[];
  /** Recent dirs for the New-thread dropdown. */
  workspaces: Workspace[];
  currentId: string | null;
  busy: boolean;
  onSelect: (id: string) => void;
  onRename: (id: string, title: string) => void;
  onDelete: (id: string) => void;
  /** No cwd → App opens the picker; with cwd → create in that dir. */
  onNew: (cwd?: string) => void;
  onRemoveWorkspace: (cwd: string) => void;
  /** Open the native folder picker (App adds the dir + creates a thread). */
  onBrowse: () => void;
}) {
  const [editingId, setEditingId] = useState<string | null>(null);
  const [draft, setDraft] = useState("");
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  // Self-drawn menus: the top "New thread" dropdown and the per-group context
  // menu. A fixed transparent backdrop (rendered with each menu) closes them on
  // an outside click; the two are mutually exclusive.
  const [newMenuOpen, setNewMenuOpen] = useState(false);
  const [contextWs, setContextWs] = useState<string | null>(null);
  const [width, setWidth] = useState(loadSidebarWidth);
  const widthRef = useRef(width);
  const cleanupDrag = useRef<(() => void) | null>(null);

  const closeMenus = useCallback(() => {
    setNewMenuOpen(false);
    setContextWs(null);
  }, []);

  // Escape closes whichever menu is open, regardless of what inside it (or the
  // trigger) currently has focus.
  useEffect(() => {
    if (!newMenuOpen && contextWs === null) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") closeMenus();
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [newMenuOpen, contextWs, closeMenus]);

  const commit = (id: string) => {
    const title = draft.trim();
    if (title) onRename(id, title);
    setEditingId(null);
  };

  const toggleCollapse = (ws: string) => {
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(ws)) next.delete(ws);
      else next.add(ws);
      return next;
    });
  };

  const renderThread = (t: ThreadSummary) => {
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
              className="pointer-events-none shrink-0 rounded px-1 text-neutral-500 opacity-0 group-hover:pointer-events-auto group-hover:opacity-100 hover:text-red-400 focus-visible:pointer-events-auto focus-visible:opacity-100 disabled:opacity-50"
            >
              ×
            </button>
          </>
        )}
      </div>
    );
  };

  return (
    <aside
      className="relative flex shrink-0 flex-col border-r border-neutral-800 bg-neutral-900"
      style={{ width }}
    >
      <div className="relative p-2">
        <button
          type="button"
          aria-haspopup="menu"
          aria-expanded={newMenuOpen}
          onClick={() => {
            // No recent dirs to offer: go straight to the native picker.
            if (workspaces.length === 0) {
              onBrowse();
              return;
            }
            setContextWs(null);
            setNewMenuOpen((v) => !v);
          }}
          disabled={busy}
          className="w-full rounded-md bg-neutral-800 px-3 py-2 text-left text-sm text-neutral-100 hover:bg-neutral-700 disabled:opacity-50"
        >
          + New thread
        </button>
        {newMenuOpen && (
          <>
            <div className="fixed inset-0 z-10" onClick={closeMenus} />
            <div
              role="menu"
              className="absolute right-2 left-2 top-full z-20 -mt-1 rounded-md border border-neutral-700 bg-neutral-800 py-1 shadow-xl"
            >
              {workspaces.map((w) => (
                <button
                  key={w.path}
                  type="button"
                  role="menuitem"
                  disabled={!w.exists}
                  title={w.path}
                  onClick={() => {
                    closeMenus();
                    onNew(w.path);
                  }}
                  className={`block w-full truncate px-3 py-1.5 text-left text-xs ${
                    w.exists
                      ? "text-neutral-200 hover:bg-neutral-700"
                      : "cursor-not-allowed text-neutral-600"
                  }`}
                >
                  {basename(w.path)}
                </button>
              ))}
              <button
                type="button"
                role="menuitem"
                onClick={() => {
                  closeMenus();
                  onBrowse();
                }}
                className="block w-full px-3 py-1.5 text-left text-xs text-neutral-300 hover:bg-neutral-700"
              >
                Browse…
              </button>
            </div>
          </>
        )}
      </div>
      <div className="flex-1 overflow-y-auto">
        {groupCount(groups) === 0 && (
          <p className="px-3 py-2 text-xs text-neutral-600">选择一个目录开始</p>
        )}
        {groups.map((g) => {
          const isCollapsed = collapsed.has(g.workspace);
          return (
            <div key={g.workspace} className="relative">
              <div
                tabIndex={0}
                aria-haspopup="menu"
                aria-expanded={contextWs === g.workspace}
                onContextMenu={(e) => {
                  e.preventDefault();
                  if (busy) return;
                  setNewMenuOpen(false);
                  setContextWs(g.workspace);
                }}
                onKeyDown={(e) => {
                  // 键盘等价于右键:Enter/Space 打开(或关闭)该组的菜单。
                  // 只处理组头自身获焦的情况——caret 按钮是子元素,其 keydown 会
                  // 冒泡到此处;若在此 preventDefault 会吞掉 caret 的 Enter/Space 激活,
                  // 导致键盘无法折叠分组(回归)。
                  if (e.target !== e.currentTarget) return;
                  if (e.key !== "Enter" && e.key !== " ") return;
                  if (busy) return;
                  e.preventDefault();
                  setNewMenuOpen(false);
                  setContextWs((cur) => (cur === g.workspace ? null : g.workspace));
                }}
                className="flex items-center gap-1 px-2 py-1.5 text-xs text-neutral-500 hover:bg-neutral-800/50 focus:bg-neutral-800/50 focus:outline-none"
              >
                <button
                  type="button"
                  onClick={() => toggleCollapse(g.workspace)}
                  aria-label={isCollapsed ? "Expand" : "Collapse"}
                  title={isCollapsed ? "Expand" : "Collapse"}
                  className="shrink-0 px-0.5 text-neutral-500 hover:text-neutral-300"
                >
                  {isCollapsed ? "▸" : "▾"}
                </button>
                <div
                  className={`min-w-0 flex-1 truncate ${
                    g.exists ? "text-neutral-400" : "text-neutral-600"
                  }`}
                  title={g.workspace}
                >
                  {basename(g.workspace)}
                  {!g.exists && " (missing)"}
                </div>
              </div>
              {contextWs === g.workspace && (
                <>
                  <div className="fixed inset-0 z-10" onClick={closeMenus} />
                  <div
                    role="menu"
                    className="absolute top-7 left-4 z-20 rounded-md border border-neutral-700 bg-neutral-800 py-1 shadow-xl"
                  >
                    <button
                      type="button"
                      role="menuitem"
                      disabled={busy}
                      onClick={() => {
                        closeMenus();
                        onNew(g.workspace);
                      }}
                      className="block w-full px-3 py-1.5 text-left text-xs whitespace-nowrap text-neutral-200 hover:bg-neutral-700 disabled:opacity-50"
                    >
                      New thread here
                    </button>
                    <button
                      type="button"
                      role="menuitem"
                      disabled={busy}
                      onClick={() => {
                        closeMenus();
                        onRemoveWorkspace(g.workspace);
                      }}
                      className="block w-full px-3 py-1.5 text-left text-xs whitespace-nowrap text-neutral-200 hover:bg-neutral-700 disabled:opacity-50"
                    >
                      Remove from list
                    </button>
                  </div>
                </>
              )}
              {!isCollapsed && g.threads.map(renderThread)}
            </div>
          );
        })}
      </div>
      <div
        role="separator"
        aria-orientation="vertical"
        aria-label="Resize sidebar"
        className="absolute inset-y-0 right-0 z-30 w-1.5 cursor-col-resize hover:bg-neutral-700/50"
      />
    </aside>
  );
}
