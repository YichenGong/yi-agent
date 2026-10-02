import { memo, useCallback, useEffect, useRef, useState } from "react";
import type { ThreadSummary, ThreadStatus, TurnStatus, Workspace, WorkspaceGroup } from "../lib/protocol";
import { basename, groupCount } from "../lib/workspaceGroups";
import { clampSidebarWidth, loadSidebarWidth, saveSidebarWidth } from "../lib/sidebarWidth";
import { isImeEnter, useImeGuard } from "../lib/imeEnter";

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

function unreadDotClass(status: TurnStatus): string {
  if (status === "failed") return "bg-red-400";
  if (status === "interrupted") return "bg-fg-muted";
  return "bg-blue-400";
}

/**
 * The per-thread status badge.
 *
 * `running` and `awaiting_approval` must render the *same* element. The server
 * flips a thread through running → awaiting_approval → running once per
 * permission prompt; two mutually exclusive elements would unmount/remount the
 * node on every flip, restarting the CSS animation from `currentTime = 0`. That
 * restart is what makes the spinner look like it "plays a short segment then
 * loops". One span, class-only variation, keeps the animation timeline intact.
 *
 * `memo` compares the semantic status only: `App` rebuilds the `statuses` Map on
 * every notification, so a default (identity) comparison would never hit.
 */
export const StatusBadge = memo(
  function StatusBadge({ status }: { status: ThreadStatus }) {
    if (status === "idle") return null;
    // `animate-spin` stays applied in *both* states on purpose. Removing it while
    // awaiting approval (and re-adding it afterwards) drops and recreates the
    // CSS animation, resetting `currentTime` — the very "plays a short segment
    // then loops" symptom. A rotating perfect circle is visually static, so the
    // amber dot loses nothing by keeping the animation alive, and the timeline
    // survives the whole running → awaiting_approval → running flip.
    return (
      <span
        role="img"
        aria-label="Thread status"
        data-status={status}
        title={status === "running" ? "Running" : "Awaiting approval"}
        className={`shrink-0 animate-spin rounded-full ${
          status === "running"
            ? "size-3 border-2 border-line-strong border-t-fg"
            : "size-2 bg-amber-400"
        }`}
      />
    );
  },
  (prev, next) => prev.status === next.status,
);

export function ThreadSidebar({
  groups,
  workspaces,
  currentId,
  pinned,
  statuses,
  unread,
  onSelect,
  onRename,
  onDelete,
  onTogglePin,
  onReorderPinned,
  onNew,
  onRemoveWorkspace,
  onBrowse,
  onOpenSettings,
}: {
  groups: WorkspaceGroup[];
  /** Recent dirs for the New-thread dropdown. */
  workspaces: Workspace[];
  currentId: string | null;
  /** 服务端排好序的置顶会话（从顶到底）；置顶分区按此顺序渲染。 */
  pinned: ThreadSummary[];
  statuses: Map<string, ThreadStatus>;
  /** thread_id → 最近一轮结束状态；含 key 即有未读点。 */
  unread: Map<string, TurnStatus>;
  onSelect: (id: string) => void;
  onRename: (id: string, title: string) => void;
  onDelete: (id: string) => void;
  onTogglePin: (id: string, pinned: boolean) => void;
  /** 置顶分区拖拽落点：新的从顶到底顺序。 */
  onReorderPinned: (orderedIds: string[]) => void;
  /** No cwd → App opens the picker; with cwd → create in that dir. */
  onNew: (cwd?: string) => void;
  onRemoveWorkspace: (cwd: string) => void;
  /** Open the native folder picker (App adds the dir + creates a thread). */
  onBrowse: () => void;
  onOpenSettings: () => void;
}) {
  const [editingId, setEditingId] = useState<string | null>(null);
  const [draft, setDraft] = useState("");
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  // 置顶分区自身的折叠状态（与工作区分组的折叠互不影响）。
  const [pinnedCollapsed, setPinnedCollapsed] = useState(false);
  // 置顶分区内的拖拽：拖动中的 thread_id + 当前悬停的行下标。
  const [dragId, setDragId] = useState<string | null>(null);
  const [dropIndex, setDropIndex] = useState<number | null>(null);
  // Self-drawn menus: the top "New thread" dropdown and the per-group context
  // menu. A fixed transparent backdrop (rendered with each menu) closes them on
  // an outside click; the two are mutually exclusive.
  const [newMenuOpen, setNewMenuOpen] = useState(false);
  const [contextWs, setContextWs] = useState<string | null>(null);
  const [width, setWidth] = useState(loadSidebarWidth);
  const widthRef = useRef(width);
  const cleanupDrag = useRef<(() => void) | null>(null);
  // The rename field is a plain text input, so an IME confirm-Enter must not
  // commit the draft (and unmount the field) mid-composition.
  const ime = useImeGuard();

  const onHandleDown = (e: React.MouseEvent) => {
    e.preventDefault();
    // A prior drag may have missed its mouseup (e.g. released outside the window);
    // unwind it before starting a new one so listeners/body styles can't stack.
    cleanupDrag.current?.();
    const startX = e.clientX;
    const startWidth = widthRef.current;
    const prevCursor = document.body.style.cursor;
    const prevUserSelect = document.body.style.userSelect;

    const onMove = (ev: MouseEvent) => {
      const next = clampSidebarWidth(startWidth + (ev.clientX - startX));
      widthRef.current = next;
      setWidth(next);
    };

    const cleanup = () => {
      document.removeEventListener("mousemove", onMove);
      document.removeEventListener("mouseup", onUp);
      window.removeEventListener("blur", onBlur);
      document.body.style.cursor = prevCursor;
      document.body.style.userSelect = prevUserSelect;
      cleanupDrag.current = null;
    };

    const onUp = () => {
      cleanup();
      saveSidebarWidth(widthRef.current);
    };

    const onBlur = () => {
      cleanup();
      saveSidebarWidth(widthRef.current);
    };

    document.body.style.cursor = "col-resize";
    document.body.style.userSelect = "none";
    document.addEventListener("mousemove", onMove);
    document.addEventListener("mouseup", onUp);
    window.addEventListener("blur", onBlur);
    cleanupDrag.current = cleanup;
  };

  // Remove any lingering document listeners if we unmount mid-drag.
  useEffect(() => () => cleanupDrag.current?.(), []);

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

  /**
   * 落点提交：把拖动中的行移到 `index` 处；顺序真的变了才回调。
   *
   * 语义 = 「占住悬停行的位置」：`index` 是悬停行在**原始**列表里的下标。
   * 先摘下被拖动项再插入，故插入位置就是 `index`（`index` 落在尾部时 clamp）。
   * 悬停行原本在拖动项下方时会左移一位，于是被拖动项落在它之后；反之落在它之前。
   * 这样「把第一行拖到第二行」得到交换，而不是原地不动；拖到自己身上是 no-op。
   */
  const commitPinDrop = (index: number) => {
    const from = pinned.findIndex((t) => t.thread_id === dragId);
    setDragId(null);
    setDropIndex(null);
    if (from < 0) return;
    const ids = pinned.map((t) => t.thread_id);
    const [moved] = ids.splice(from, 1);
    const to = Math.min(Math.max(index, 0), ids.length);
    ids.splice(to, 0, moved);
    if (ids.join(",") !== pinned.map((t) => t.thread_id).join(",")) onReorderPinned(ids);
  };

  // 拖动中的行下标：既用于指示线方向（向上/向下），也用于抑制"落在自己身上"的指示线。
  const dragFromIndex = dragId === null ? -1 : pinned.findIndex((t) => t.thread_id === dragId);

  const renderThread = (
    t: ThreadSummary,
    opts: { rowAttr?: "group" | "pinned"; dragIndex?: number } = {},
  ) => {
    const active = t.thread_id === currentId;
    const st = statuses.get(t.thread_id) ?? "idle";
    const un = unread.get(t.thread_id);
    const isPinned = t.pinned ?? false;
    const dragging = opts.rowAttr === "pinned";
    const dataAttr =
      opts.rowAttr === "pinned" ? { "data-pinned-row": "" } : { "data-group-row": "" };
    return (
      <div
        key={t.thread_id}
        {...dataAttr}
        // 只有置顶分区的行可拖拽；编辑标题态不可拖（否则拖动会打断输入）。
        draggable={dragging && editingId !== t.thread_id}
        onDragStart={dragging ? () => setDragId(t.thread_id) : undefined}
        onDragOver={
          dragging
            ? (e) => {
                if (dragId === null) return;
                // 必须 preventDefault，否则浏览器不认这个落点（也不发 drop）。
                e.preventDefault();
                setDropIndex(opts.dragIndex ?? null);
              }
            : undefined
        }
        onDrop={
          dragging
            ? (e) => {
                e.preventDefault();
                commitPinDrop(opts.dragIndex ?? 0);
              }
            : undefined
        }
        onDragEnd={
          dragging
            ? () => {
                setDragId(null);
                setDropIndex(null);
              }
            : undefined
        }
        onClick={(e) => {
          if (editingId) return;
          // 双击会先派发两次 click 再派发 dblclick;忽略第二次 click,
          // 避免对同一 thread 触发两次 onSelect(即两次 thread/resume)。
          if (e.detail > 1) return;
          onSelect(t.thread_id);
        }}
        className={`group relative flex items-center justify-between gap-1 px-3 py-2 text-sm ${
          dragId === t.thread_id ? "opacity-50" : ""
        } ${active ? "bg-raised text-fg" : "text-fg-muted hover:bg-raised/50"} cursor-pointer`}
      >
        {editingId === t.thread_id ? (
          <input
            autoFocus
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            onCompositionStart={ime.onCompositionStart}
            onCompositionEnd={ime.onCompositionEnd}
            onBlur={() => {
              ime.resetComposition();
              commit(t.thread_id);
            }}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !isImeEnter(e, ime.composing.current)) commit(t.thread_id);
              if (e.key === "Escape") setEditingId(null);
            }}
            className="w-full rounded bg-surface px-1 py-0.5 text-sm text-fg outline-none"
          />
        ) : (
          <>
            {dragging && dropIndex === opts.dragIndex && dragId !== null && dragFromIndex >= 0 && (
              <div
                data-pin-drop-indicator=""
                className={`absolute inset-x-0 h-0.5 bg-amber-400 ${
                  dragFromIndex < opts.dragIndex! ? "bottom-0" : "top-0"
                }`}
              />
            )}
            <div className="min-w-0 flex-1">
              <div className="flex items-center gap-1.5">
                <StatusBadge status={st} />
                <div
                  className="truncate"
                  title={t.title ?? t.thread_id}
                  onDoubleClick={() => {
                    setEditingId(t.thread_id);
                    setDraft(t.title ?? "");
                  }}
                >
                  {t.title ?? "(untitled)"}
                </div>
              </div>
              <div className="text-xs text-fg-faint">{relativeTime(t.updated_at)}</div>
            </div>
            {un && !active && (
              <span
                aria-label="Unread"
                title="Unread result"
                className={`size-2 shrink-0 rounded-full ${unreadDotClass(un)}`}
              />
            )}
            <button
              type="button"
              aria-label={isPinned ? "Unpin thread" : "Pin thread"}
              aria-pressed={isPinned}
              title={isPinned ? "Unpin" : "Pin"}
              onClick={(e) => {
                e.stopPropagation();
                onTogglePin(t.thread_id, !isPinned);
              }}
              className={`shrink-0 rounded px-1 ${
                isPinned
                  ? "text-amber-400 hover:text-amber-300"
                  : "pointer-events-none text-fg-subtle opacity-0 group-hover:pointer-events-auto group-hover:opacity-100 hover:text-amber-300 focus-visible:pointer-events-auto focus-visible:opacity-100"
              }`}
            >
              <svg viewBox="0 0 16 16" className="size-3.5" fill="currentColor" aria-hidden="true">
                <path d="M9.5 1 15 6.5l-2.6.6-1 3.1-2.6-2.6L4.3 12 3 13l1-4.4L6.6 6 4 3.4l3.1-1L9.5 1Z" />
              </svg>
            </button>
            <button
              type="button"
              onClick={(e) => {
                e.stopPropagation();
                onDelete(t.thread_id);
              }}
              title="Delete"
              aria-label="Delete thread"
              className="pointer-events-none shrink-0 rounded px-1 text-fg-subtle opacity-0 group-hover:pointer-events-auto group-hover:opacity-100 hover:text-red-400 focus-visible:pointer-events-auto focus-visible:opacity-100"
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
      className="relative flex shrink-0 flex-col border-r border-line bg-panel"
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
          className="w-full rounded-md bg-raised px-3 py-2 text-left text-sm text-fg hover:bg-raised"
        >
          + New thread
        </button>
        {newMenuOpen && (
          <>
            <div className="fixed inset-0 z-10" onClick={closeMenus} />
            <div
              role="menu"
              className="absolute right-2 left-2 top-full z-20 -mt-1 rounded-md border border-line-strong bg-raised py-1 shadow-xl"
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
                      ? "text-fg hover:bg-raised"
                      : "cursor-not-allowed text-fg-faint"
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
                className="block w-full px-3 py-1.5 text-left text-xs text-fg-muted hover:bg-raised"
              >
                Browse…
              </button>
            </div>
          </>
        )}
      </div>
      <div className="flex-1 overflow-y-auto">
        {groupCount(groups) === 0 && (
          <p className="px-3 py-2 text-xs text-fg-faint">选择一个目录开始</p>
        )}
        {pinned.length > 0 && (
          <div>
            <div className="flex items-center gap-1 px-2 py-1.5 text-xs text-fg-subtle">
              <button
                type="button"
                onClick={() => setPinnedCollapsed((v) => !v)}
                aria-label={pinnedCollapsed ? "Expand pinned" : "Collapse pinned"}
                title={pinnedCollapsed ? "Expand" : "Collapse"}
                className="shrink-0 px-0.5 text-fg-subtle hover:text-fg-muted"
              >
                {pinnedCollapsed ? "▸" : "▾"}
              </button>
              <div className="min-w-0 flex-1 truncate text-fg-muted">Pinned</div>
            </div>
            {!pinnedCollapsed &&
              pinned.map((t, i) => renderThread(t, { rowAttr: "pinned", dragIndex: i }))}
          </div>
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
                  e.preventDefault();
                  setNewMenuOpen(false);
                  setContextWs((cur) => (cur === g.workspace ? null : g.workspace));
                }}
                className="flex items-center gap-1 px-2 py-1.5 text-xs text-fg-subtle hover:bg-raised/50 focus:bg-raised/50 focus:outline-none"
              >
                <button
                  type="button"
                  onClick={() => toggleCollapse(g.workspace)}
                  aria-label={isCollapsed ? "Expand" : "Collapse"}
                  title={isCollapsed ? "Expand" : "Collapse"}
                  className="shrink-0 px-0.5 text-fg-subtle hover:text-fg-muted"
                >
                  {isCollapsed ? "▸" : "▾"}
                </button>
                <div
                  className={`min-w-0 flex-1 truncate ${
                    g.exists ? "text-fg-muted" : "text-fg-faint"
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
                    className="absolute top-7 left-4 z-20 rounded-md border border-line-strong bg-raised py-1 shadow-xl"
                  >
                    <button
                      type="button"
                      role="menuitem"
                      onClick={() => {
                        closeMenus();
                        onNew(g.workspace);
                      }}
                      className="block w-full px-3 py-1.5 text-left text-xs whitespace-nowrap text-fg hover:bg-raised"
                    >
                      New thread here
                    </button>
                    <button
                      type="button"
                      role="menuitem"
                      onClick={() => {
                        closeMenus();
                        onRemoveWorkspace(g.workspace);
                      }}
                      className="block w-full px-3 py-1.5 text-left text-xs whitespace-nowrap text-fg hover:bg-raised"
                    >
                      Remove from list
                    </button>
                  </div>
                </>
              )}
              {!isCollapsed &&
                g.threads
                  .filter((t) => !t.pinned)
                  .map((t) => renderThread(t, { rowAttr: "group" }))}
            </div>
          );
        })}
      </div>
      <div className="mt-auto flex items-center border-t border-line p-2">
        <button
          type="button"
          aria-label="设置"
          title="设置"
          onClick={onOpenSettings}
          className="rounded p-1 text-fg-subtle hover:bg-raised/50 hover:text-fg"
        >
          <svg viewBox="0 0 16 16" className="size-4" fill="currentColor" aria-hidden="true">
            <path d="M8 10.5a2.5 2.5 0 1 0 0-5 2.5 2.5 0 0 0 0 5Z" />
            <path d="M6.9 1.3a.7.7 0 0 0-.7.6l-.1.9a5.4 5.4 0 0 0-.9.5l-.9-.3a.7.7 0 0 0-.8.3l-.7 1.2a.7.7 0 0 0 .1.9l.7.6a5.5 5.5 0 0 0 0 1l-.7.6a.7.7 0 0 0-.1.9l.7 1.2a.7.7 0 0 0 .8.3l.9-.3c.3.2.6.4.9.5l.1.9a.7.7 0 0 0 .7.6h1.4a.7.7 0 0 0 .7-.6l.1-.9c.3-.1.6-.3.9-.5l.9.3a.7.7 0 0 0 .8-.3l.7-1.2a.7.7 0 0 0-.1-.9l-.7-.6a5.5 5.5 0 0 0 0-1l.7-.6a.7.7 0 0 0 .1-.9l-.7-1.2a.7.7 0 0 0-.8-.3l-.9.3a5.4 5.4 0 0 0-.9-.5l-.1-.9a.7.7 0 0 0-.7-.6H6.9Zm1.1 8a1.6 1.6 0 1 1 0-3.2 1.6 1.6 0 0 1 0 3.2Z" />
          </svg>
        </button>
      </div>
      <div
        role="separator"
        aria-orientation="vertical"
        aria-label="Resize sidebar"
        onMouseDown={onHandleDown}
        className="absolute inset-y-0 right-0 z-30 w-1.5 cursor-col-resize hover:bg-raised/50"
      />
    </aside>
  );
}
