import { memo, useCallback, useEffect, useMemo, useRef, useState } from "react";
import type {
  SessionGroup,
  ThreadSummary,
  ThreadStatus,
  TurnStatus,
  Workspace,
  WorkspaceGroup,
} from "../lib/protocol";
import { layoutWorkspace } from "../lib/sessionGroups";

import { basename, groupCount, nestLinkedGroups } from "../lib/workspaceGroups";
import { kanbanItemFor } from "../lib/boardIndex";
import { useSpinPhase } from "../lib/useSpinPhase";
import { countRunningSessions, runningSessionsLabel } from "../lib/runningSessions";
import { supportsWaapi } from "../lib/spinPhase";
import { clampSidebarWidth, loadSidebarWidth, saveSidebarWidth } from "../lib/sidebarWidth";
import { isImeEnter, useImeGuard } from "../lib/imeEnter";
import { useLongPress, type LongPressTarget } from "../lib/useLongPress";
import { resolveSidebarPressTarget } from "../lib/sidebarPress";
import { formatRelativeTime, RELATIVE_TIME_UNKNOWN } from "../lib/relativeTime";
import { filterThreadTree } from "../lib/threadSearch";
import { Popover } from "../ui/Popover";
import { findLeadThread } from "../lib/leadThread";
import { LeadAvatar } from "./LeadAvatar";
import { TAG_DOT_CLASS, TAG_IDS, isValidTagId, tagLabel, type TagId } from "../lib/threadTags";

/** 与后端 `group_store::{NAME_MAX_CHARS, DESCRIPTION_MAX_CHARS}` 一致。 */
const GROUP_NAME_MAX = 24;
const GROUP_DESCRIPTION_MAX = 120;

function unreadDotClass(status: TurnStatus): string {
  if (status === "failed") return "bg-red-400";
  if (status === "interrupted") return "bg-fg-muted";
  return "bg-blue-400";
}

/**
 * 侧栏行下方那行紧凑时间。
 *
 * 时间未知时留空而不是写出「未知」：这一列只有几个字符宽，右侧紧贴未读点，
 * 一行字会把标题挤窄；而会话本身仍然可用，缺一个时间戳不需要额外解释。
 * 关键是不能编造数字——0 会被 `formatRelativeTime` 判为未知，旧实现则算出
 * 20758d（用户报的「20xxxd」）。
 */
function sidebarAge(updatedAt: number): string {
  const label = formatRelativeTime(updatedAt);
  return label === RELATIVE_TIME_UNKNOWN ? "" : label;
}

/**
 * The per-thread status badge.
 *
 * Two independent restart hazards, both now handled:
 *
 * 1. **Status flips** (running → awaiting_approval → running once per permission
 *    prompt): handled by rendering the *same* element in both states instead of
 *    two mutually exclusive ones.
 * 2. **Row reorders**: the sidebar list is server-sorted by `updated_at` desc, so
 *    whenever any thread's `updated_at` bumps (another thread finishes a turn,
 *    gets renamed, changes model, …) this row is *moved* in the DOM — React
 *    applies a keyed reorder as `insertBefore` on a mounted node. A CSS animation
 *    restarts from `currentTime = 0` on any such move, which snaps the ring back
 *    to 12 o'clock: the visible "回退". Preserving the node's *identity* does not
 *    help (identity is kept, the timeline still resets).
 *
 * So the rotation is driven by the **Web Animations API** via `useSpinPhase`
 * instead of the CSS `animate-spin`: a WAAPI animation is tied to the element and
 * does *not* restart on a DOM move, and its phase is anchored to the wall clock,
 * so even a genuine remount resumes in phase. `animate-spin` is kept on the class
 * only as a fallback for environments without WAAPI (e.g. jsdom); where WAAPI
 * exists it wins the cascade for `transform`.
 *
 * `memo` compares the semantic status only: `App` rebuilds the `statuses` Map on
 * every notification, so a default (identity) comparison would never hit.
 */
export const StatusBadge = memo(function StatusBadge({
  status,
  stale = false,
}: {
  status: ThreadStatus;
  stale?: boolean;
}) {
  const ref = useRef<HTMLSpanElement>(null);
  useSpinPhase(ref, status !== "idle");
  if (status === "idle") return null;
  const label = status === "running" ? "Running" : "Awaiting approval";
  // Exactly one mechanism drives the spin. Where WAAPI exists it is the driver
  // and the CSS class is omitted (two animations on `transform` would be
  // ambiguous and redundant); where it does not (jsdom, ancient webviews) we fall
  // back to the CSS `animate-spin` so the indicator still rotates — losing only
  // the "survives a row reorder" property.
  const cssFallback = !supportsWaapi();
  return (
    <span
      ref={ref}
      role="img"
      aria-label="Thread status"
      data-status={status}
      data-stale={stale ? "true" : undefined}
      title={stale ? `${label}（可能过期）` : label}
      className={`shrink-0 rounded-full ${cssFallback ? "animate-spin" : ""} ${
        stale ? "opacity-40" : ""
      } ${status === "running" ? "size-3 border-2 border-line-strong border-t-fg" : "size-2 bg-amber-400"}`}
    />
  );
}, (prev, next) => prev.status === next.status && prev.stale === next.stale);

/**
 * footer 里的「此刻有几个会话在跑」读数,紧挨设置齿轮。
 *
 * 与 `StatusBadge` 共用 `useSpinPhase`(相位锚定墙钟),避免 CSS 动画在节点重排时
 * 从头播放的视觉回退。恒为 running 形态:它只在计数 > 0 时出现,不存在
 * `awaiting_approval` 形态(那种会话根本不计入)。
 *
 * 计数为 0 时整个节点不渲染,footer 与改动前逐像素一致。
 */
function ActiveSessionCount({ count, stale = false }: { count: number; stale?: boolean }) {
  const ref = useRef<HTMLSpanElement>(null);
  useSpinPhase(ref, count > 0);
  if (count === 0) return null;
  const label = runningSessionsLabel(count);
  const cssFallback = !supportsWaapi();
  return (
    <span
      role="status"
      aria-live="polite"
      aria-label={label}
      title={stale ? `${label}（可能过期）` : label}
      className={`ml-2 flex min-w-0 items-center gap-1.5 text-xs text-fg-subtle ${
        stale ? "opacity-40" : ""
      }`}
    >
      <span
        ref={ref}
        aria-hidden="true"
        className={`shrink-0 rounded-full ${cssFallback ? "animate-spin" : ""} size-3 border-2 border-line-strong border-t-fg`}
      />
      <span className="truncate">{label}</span>
    </span>
  );
}

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
  onSetTag,
  onCreateGroup,
  onUpdateGroup,
  onSetGroupMembers,
  onDeleteGroup,
  onError,
  tagNames,
  onNew,
  onRemoveWorkspace,
  onBrowse,
  boards,
  boardSummaries,
  selectedBoard,
  onCreateBoard,
  onRemoveBoard,
  onOpenBoard,
  onOpenSettings,
  onLinkFolder,
  onUnlinkFolder,
  isMobile = false,
  statusStale = false,
  leadAgentEnabled = true,
  kanbanEnabled = true,
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
  /** 给会话打 / 清颜色标签（`TagId` 或 null=清除）。 */
  onSetTag: (id: string, tag: TagId | null) => void;
  /** 用选中的会话建组。成功返回新组 id，null 表示失败（保留编辑器）。 */
  onCreateGroup: (
    workspace: string,
    name: string,
    description: string,
    threadIds: string[],
  ) => Promise<string | null>;
  /** 改组名 / 描述。`patch` 只带要改的字段。 */
  onUpdateGroup: (
    workspace: string,
    groupId: string,
    patch: { name?: string; description?: string },
  ) => Promise<boolean>;
  onSetGroupMembers: (workspace: string, groupId: string, threadIds: string[]) => Promise<boolean>;
  onDeleteGroup: (workspace: string, groupId: string) => Promise<boolean>;
  /** 侧栏自身的动作错误（如跨目录建组），落进会话错误区。 */
  onError: (message: string) => void;
  /** 已解析的标签显示名（6 键齐全，由 App 经 `resolveTagNames` 得到）。 */
  tagNames: Record<TagId, string>;
  /** No cwd → App opens the picker; with cwd → create in that dir. */
  onNew: (cwd?: string) => void;
  onRemoveWorkspace: (cwd: string) => void;
  /** Open the native folder picker (App adds the dir + creates a thread). */
  onBrowse: () => void;
  /** 登记了看板的项目路径；决定哪些分组画看板条目、菜单给创建还是移除。 */
  boards: string[];
  /** 项目路径 → 卡片摘要（如 "2 排队 · 1 运行中"）。 */
  boardSummaries: Record<string, string>;
  /** 当前在主区域展示看板的项目（点会话会离开看板）。 */
  selectedBoard: string | null;
  onCreateBoard: (path: string) => void;
  onRemoveBoard: (path: string) => void;
  onOpenBoard: (path: string) => void;
  onOpenSettings: () => void;
  /** 把某个主根与用户选中的目录关联（由 App 弹原生目录选择器）。 */
  onLinkFolder: (primary: string) => void;
  /** 解除某个附属根的关联。 */
  onUnlinkFolder: (child: string) => void;
  /** 手机端才挂长按（= 右键）；桌面端保持现状，不装监听、不渲染会话菜单。 */
  isMobile?: boolean;
  /** 内容来自缓存、尚未与服务器对齐时置 true：徽标降透明并标注"可能过期"。
   * 仅手机端离线时为 true；桌面端恒为 false，渲染逐字节不变。 */
  statusStale?: boolean;
  /**
   * 组长功能是否开启（`flags/list` 里的 `lead_agent`）。
   *
   * 缺省 `true`：这个组件的既有用法（含测试）都不传它，缺省成 `false` 会让
   * 所有既有侧栏渲染突然少一行组长。生产路径由 `App` 显式传入真实开关值。
   */
  leadAgentEnabled?: boolean;
  /**
   * 整个 Superpowers 看板是否开启（`flags/list` 里的 `superpowers_kanban`）。
   *
   * 关掉时这里**什么都不画**：看板入口、菜单里的「创建/移除看板」，以及所有
   * `driver_ref` 非空的卡片会话（含置顶区）——包括已经存在的。理由是这个开关
   * 停掉的是整个 daemon，留着入口只会点出 `plugin_disabled`。
   *
   * 缺省 `true`，与 `leadAgentEnabled` 同一理由：既有用法与测试不传它。
   */
  kanbanEnabled?: boolean;
}) {
  const [editingId, setEditingId] = useState<string | null>(null);
  const [draft, setDraft] = useState("");
  // 侧栏搜索关键词。仅影响渲染，不进 ThreadStore，也不落盘。
  const [query, setQuery] = useState("");
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  // 置顶分区自身的折叠状态（与工作区分组的折叠互不影响）。
  const [pinnedCollapsed, setPinnedCollapsed] = useState(false);
  // 「看板会话」小节的折叠状态：独立于项目组的 `collapsed`，键为 workspace。
  const [boardCollapsed, setBoardCollapsed] = useState<Set<string>>(new Set());
  const toggleBoardCollapse = (ws: string) => {
    setBoardCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(ws)) next.delete(ws);
      else next.add(ws);
      return next;
    });
  };
  /** 判定「这是拖拽而不是点击」的位移阈值（CSS 像素）。 */
  const DRAG_THRESHOLD_PX = 4;
  // 置顶分区内的拖拽：拖动中的 thread_id + 当前悬停的行下标。
  const [dragId, setDragId] = useState<string | null>(null);
  const [dropIndex, setDropIndex] = useState<number | null>(null);
  /**
   * 指针拖拽的起手：按在哪一行、按下点在哪。
   *
   * 为什么整块不用 HTML5 拖放（`draggable` + `dragstart`）：Tauri v2 默认开启
   * `dragDropEnabled`，在 macOS 上给 WKWebView 装了原生拖放处理器，它对任何拖拽
   * 都返回「已消费」且不再调 super，于是 WebKit 自己永远收不到落点——`dragstart`
   * 照常触发，但 `dragover` / `drop` 一个都不派发，表现成「能拿起来、放不下去」。
   * 关掉它又会丢掉「从访达拖路径进输入框」，故改走指针事件：不经过原生拖放会话。
   */
  const [dragOrigin, setDragOrigin] = useState<{
    id: string;
    kind: "group" | "pinned";
    x: number;
    y: number;
    pointerId: number;
  } | null>(null);
  /** 是否已越过阈值（越过之后才是拖拽，之前仍算点击）。 */
  const [dragActive, setDragActive] = useState(false);
  // Self-drawn menus: the top "New thread" dropdown and the per-group context
  // menu. A fixed transparent backdrop (rendered with each menu) closes them on
  // an outside click; the two are mutually exclusive.
  const [newMenuOpen, setNewMenuOpen] = useState(false);
  // 侧栏自绘菜单：新建下拉（newMenuOpen）与长按/右键菜单（menu）三者互斥。
  // menu 用联合类型：`group` 是桌面右键那份工作区菜单，`thread` 是手机长按新增的
  // 会话菜单——同一个槽位保证两种菜单不会同时开着。
  const [menu, setMenu] = useState<
    | { kind: "group"; ws: string }
    | { kind: "child"; ws: string }
    | { kind: "thread"; id: string }
    | { kind: "groupBox"; ws: string; groupId: string }
    | null
  >(null);
  // 组框的展开集合（键为 group id）：与 workspace 折叠分开，折叠目录不等于折叠组。
  const [expandedGroups, setExpandedGroups] = useState<Set<string>>(new Set());
  /**
   * 是否有修饰键（Cmd/Ctrl/Shift）正被按住。
   *
   * 多选靠点击攒起来。拖拽若在按着修饰键时也能起手，「按下 + 轻微移动」就会
   * 被当成拖拽起点、click 随之消失——「点了三条只记到两条」正是这条路。按下
   * 修饰键时不起手拖拽，点击语义回到加手势之前。
   *
   * 用 `e.key` 判定而不是 `e.metaKey`：`keyup` 时 `metaKey` 已经为 false，
   * 读不到「刚才松开的是谁」。
   */
  const [modifierDown, setModifierDown] = useState(false);
  useEffect(() => {
    const onDown = (e: KeyboardEvent) => {
      if (e.key === "Meta" || e.key === "Control" || e.key === "Shift") setModifierDown(true);
    };
    const onUp = (e: KeyboardEvent) => {
      if (e.key === "Meta" || e.key === "Control" || e.key === "Shift") setModifierDown(false);
    };
    // 窗口失焦（Cmd+Tab 切走）后 keyup 不会到达，必须复位，否则侧栏永久不可拖。
    const onBlur = () => setModifierDown(false);
    document.addEventListener("keydown", onDown);
    document.addEventListener("keyup", onUp);
    window.addEventListener("blur", onBlur);
    return () => {
      document.removeEventListener("keydown", onDown);
      document.removeEventListener("keyup", onUp);
      window.removeEventListener("blur", onBlur);
    };
  }, []);
  // 多选：会话行的选择集。Ctrl/Cmd+点击进出，Shift+点击按当前行序区间选择。
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [selectionAnchor, setSelectionAnchor] = useState<string | null>(null);
  // 就地编辑：正在编辑的组字段与草稿。
  const [editingGroup, setEditingGroup] = useState<
    { workspace: string; groupId: string; field: "name" | "description" } | null
  >(null);
  const [groupDraft, setGroupDraft] = useState("");
  // 新建组的输入态：框属于哪个 workspace、组名与描述草稿。
  const [newGroup, setNewGroup] = useState<{
    workspace: string;
    name: string;
    description: string;
  } | null>(null);
  // 顶部 “+ New thread” 按钮的点击涟漪：按下时按指针位置生成一圈扩散，
  // `animationend` 后移除，避免节点堆积。键盘激活没有坐标，退化为居中涟漪。
  const [ripples, setRipples] = useState<{ id: number; x: number; y: number; size: number }[]>([]);
  const rippleSeq = useRef(0);
  const [width, setWidth] = useState(loadSidebarWidth);
  const widthRef = useRef(width);
  const cleanupDrag = useRef<(() => void) | null>(null);
  // The rename field is a plain text input, so an IME confirm-Enter must not
  // commit the draft (and unmount the field) mid-composition.
  const ime = useImeGuard();
  const runningCount = countRunningSessions(statuses);

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
    setMenu(null);
  }, []);

  // 长按 → 打开对应菜单。落点判定复用 sidebarPress 的标记属性；桌面端 enabled=false
  // 时 hook 不装任何监听。
  const longPress = useLongPress({
    enabled: isMobile,
    resolveTarget: (e): LongPressTarget | null => {
      const el = resolveSidebarPressTarget(e.target as Element);
      if (!el) return null;
      // 编辑态的行不弹菜单（否则长按会打断正在输入的标题）。
      if (el.dataset.threadRow !== undefined) {
        if (editingId === el.dataset.threadRow) return null;
        return { kind: "thread", id: el.dataset.threadRow, el };
      }
      // 附属根小节头：长按只给「取消关联」，不给 New thread here（附属根不能作 cwd）。
      if (el.dataset.linkedChild !== undefined) {
        return { kind: "child", ws: el.dataset.linkedChild, el };
      }
      return { kind: "group", ws: el.dataset.wsHeader ?? "", el };
    },
    onLongPress: (t) => {
      setNewMenuOpen(false);
      setMenu(
        t.kind === "thread"
          ? { kind: "thread", id: t.id }
          : t.kind === "child"
            ? { kind: "child", ws: t.ws }
            : { kind: "group", ws: t.ws },
      );
    },
  });

  // Reduced-motion users get no ripple at all (rather than a fast one): the
  // decoration is pure feedback and the click itself is unaffected.
  const reduceMotion = useCallback(() => {
    if (typeof window === "undefined" || !window.matchMedia) return false;
    return window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  }, []);

  // `rippleSeq` only ever grows, so ids are unique per mount.
  const addRipple = useCallback(
    (x: number, y: number, size: number) => {
      const id = rippleSeq.current++;
      setRipples((prev) => [...prev, { id, x, y, size }]);
    },
    [],
  );

  const removeRipple = useCallback((id: number) => {
    setRipples((prev) => prev.filter((r) => r.id !== id));
  }, []);

  /** Radius that reaches the button's farthest corner from the press point. */
  const rippleSizeFor = (rect: DOMRect, x: number, y: number) => {
    const dx = Math.max(x, rect.width - x);
    const dy = Math.max(y, rect.height - y);
    return Math.hypot(dx, dy) * 2;
  };

  const onNewThreadPointerDown = (e: React.PointerEvent<HTMLButtonElement>) => {
    if (reduceMotion()) return;
    const rect = e.currentTarget.getBoundingClientRect();
    const x = e.clientX - rect.left;
    const y = e.clientY - rect.top;
    addRipple(x, y, rippleSizeFor(rect, x, y));
  };

  // Keyboard activation has no pointer coordinates, so it gets a centred ripple.
  // Handling it on keydown (rather than guessing from `click`) keeps the pointer
  // and keyboard paths independent: a real Enter/Space fires keydown *and* the
  // browser's synthesized click, so reacting in `click` would double-fire.
  const onNewThreadKeyDown = (e: React.KeyboardEvent<HTMLButtonElement>) => {
    if (e.repeat || reduceMotion()) return;
    if (e.key !== "Enter" && e.key !== " ") return;
    const rect = e.currentTarget.getBoundingClientRect();
    const x = rect.width / 2;
    const y = rect.height / 2;
    addRipple(x, y, rippleSizeFor(rect, x, y));
  };

  const onNewThreadClick = () => {
    // No recent dirs to offer: go straight to the native picker.
    if (workspaces.length === 0) {
      onBrowse();
      return;
    }
    setMenu(null);
    setNewMenuOpen((v) => !v);
  };

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
    const from = pinned.findIndex((t) => t.thread_id === dragOrigin?.id);
    clearDrag();
    if (from < 0) return;
    const ids = pinned.map((t) => t.thread_id);
    const [moved] = ids.splice(from, 1);
    const to = Math.min(Math.max(index, 0), ids.length);
    ids.splice(to, 0, moved);
    if (ids.join(",") !== pinned.map((t) => t.thread_id).join(",")) onReorderPinned(ids);
  };

  // 拖动中的行下标：既用于指示线方向（向上/向下），也用于抑制"落在自己身上"的指示线。
  const dragFromIndex =
    dragOrigin?.kind !== "pinned" ? -1 : pinned.findIndex((t) => t.thread_id === dragOrigin.id);

  /**
   * 正在被拖动的会话（进/出组用），一批。
   *
   * 存数组而不是单条：多选时拖任意一条 = 整批操作。payload 在越过阈值那一刻
   * 快照一次，拖拽途中不再变（否则拖过界时选择集被改，落点会跟着漂）。
   */
  const [dragPayload, setDragPayload] = useState<string[] | null>(null);
  const [groupDropTarget, setGroupDropTarget] = useState<string | null>(null);
  /** 框外会话行作为「移出」落点时的悬停 id（画描边）。 */
  const [dropOverThread, setDropOverThread] = useState<string | null>(null);

  /** 复位所有拖拽态。松手、取消、提交三处都走它，避免漏掉一个状态。 */
  function clearDrag() {
    setDragOrigin(null);
    setDragActive(false);
    setDragPayload(null);
    setGroupDropTarget(null);
    setDropOverThread(null);
  }

  /** 该会话当前所在的组（多对多，取第一个；"拖出"只从它所在的框里移除）。 */
  const groupOf = (id: string) => groupsForThread(id)[0] ?? null;

  /**
   * 越过阈值：此刻才确定「拖的是哪一批」。
   *
   * 在选择集里就带上整批，否则只带这一条——用户拖了一条没选中的行，意图显然
   * 只是这一条。置顶区拖拽只带自己（它是重排，不参与成组）。
   */
  const beginDragPayload = () => {
    if (!dragOrigin) return;
    if (dragOrigin.kind === "pinned") {
      setDragId(dragOrigin.id);
      return;
    }
    setDragPayload(isSelected(dragOrigin.id) ? effectiveSelection() : [dragOrigin.id]);
  };

  /** 指针按下：记下起手行与按下点。是否成拖由 window 上的位移判定。 */
  const onRowPointerDown = (id: string, kind: "group" | "pinned", e: React.PointerEvent) => {
    // 只认主键。
    if (e.button !== 0) return;
    if (editingId === id) return;
    // 进组拖拽按着修饰键时不起手：那时用户在点选，不是在拖。
    // 置顶区不设这道门——那里拖拽就是重排，与多选不争同一个动作。
    if (kind === "group" && modifierDown) return;
    // 必须挡住原生文本拖拽：会话行标题是可选中文本，WebKit 在「按下 + 拖动」
    // 时会先启动原生 NSDragging 会话（jsdom 与普通浏览器里看不出来，因为那些
    // 环境没有 Tauri 装的原生拖放处理器）。而 Tauri 的处理器会把这次原生拖拽
    // 判为「已消费」，于是后续 pointermove/pointerup 一个都不再派发到页面——
    // 拖拽永远走不到 4px 阈值。preventDefault 让 WebKit 放弃启动该会话。
    e.preventDefault();
    // 但 preventDefault 也一并抑制了**焦点转移**，于是在组名/描述输入框里编辑
    // 时点会话行不会触发 blur，那两处「点别处 = 保存」就永远不保存。这里补上
    // 焦点语义：把焦点从正在编辑的控件上主动移走，让它的 onBlur 照常跑。
    const focused = document.activeElement;
    if (focused instanceof HTMLElement && focused !== e.currentTarget && !e.currentTarget.contains(focused)) {
      focused.blur();
    }
    setDragOrigin({ id, kind, x: e.clientX, y: e.clientY, pointerId: e.pointerId });
    setDragActive(false);
  };

  /**
   * 松手时用的最新落点。
   *
   * window 上的 listener 只在起手时装一次（deps 是起手态），读不到后续 setState 的
   * 新值；而落点正是在拖拽过程中被 pointerenter 不断更新的。用 ref 镜像一份，
   * 保证「松手那一刻」读到的是当前落点，而不是起手时的快照。
   */
  const dropTargetRef = useRef({
    groupDropTarget: null as string | null,
    dropOverThread: null as string | null,
    dropIndex: null as number | null,
    payload: null as string[] | null,
  });
  dropTargetRef.current = { groupDropTarget, dropOverThread, dropIndex, payload: dragPayload };

  // 位移判定与松手提交都挂在 window 上：拖拽中指针会离开起手行，甚至离开窗口。
  useEffect(() => {
    if (!dragOrigin) return;
    const { x, y, pointerId, kind } = dragOrigin;

    const onMove = (e: PointerEvent) => {
      if (e.pointerId !== pointerId || dragActive) return;
      // 阈值：手抖一两像素仍是点击。整行可拖之后，「拖拽抢走点击」就是这条路。
      if (Math.abs(e.clientX - x) < DRAG_THRESHOLD_PX && Math.abs(e.clientY - y) < DRAG_THRESHOLD_PX)
        return;
      setDragActive(true);
      beginDragPayload();
    };
    const onUp = (e: PointerEvent) => {
      if (e.pointerId !== pointerId) return;
      // 没越过阈值 = 一次普通点击，什么都不提交（点击本身已由 onClick 处理）。
      if (!dragActive) {
        clearDrag();
        return;
      }
      const drop = dropTargetRef.current;
      if (kind === "pinned") {
        if (drop.dropIndex !== null) commitPinDrop(drop.dropIndex);
        else clearDrag();
        return;
      }
      if (drop.groupDropTarget !== null) {
        const group = groups
          .flatMap((g) => (g.groups ?? []).map((x) => ({ workspace: g.workspace, group: x })))
          .find((entry) => entry.group.id === drop.groupDropTarget);
        if (group) dropIntoGroup(group.workspace, group.group);
        else clearDrag();
        return;
      }
      if (drop.dropOverThread !== null) {
        dropOutOfGroup();
        return;
      }
      clearDrag();
    };
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
    window.addEventListener("pointercancel", onUp);
    return () => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
      window.removeEventListener("pointercancel", onUp);
    };
  }, [dragOrigin, dragActive]);

  /**
   * 拖到组框上：批量加入该组。
   *
   * 已在组里的跳过（幂等）——重发一次仍是一次多余的写，且会让组内顺序抖动。
   * 跨 workspace 的落点直接忽略：拖拽的失败反馈应当是"没反应"，而不是一条
   * 打断操作的红字。
   */
  const dropIntoGroup = (workspace: string, group: SessionGroup) => {
    const ids = dragPayload;
    clearDrag();
    if (!ids || ids.length === 0) return;
    const add = ids.filter(
      (id) => workspaceOfThread(id) === workspace && !group.threadIds.includes(id),
    );
    if (add.length === 0) return;
    void onSetGroupMembers(workspace, group.id, [...group.threadIds, ...add]);
  };

  /**
   * 拖到框外**会话行**上：批量移出。
   *
   * 目标组 = 起手那一行所在的第一个组：多选跨越两个组时，从 A 组拖出来只解散
   * A 组那批，B 组的成员不动——"拖出这个框"就是它字面的意思。
   *
   * 落点只看会话行，目录标题与空白处都不认（它们不是会话，当落点等于让用户猜）。
   * 落点是哪一行也不影响结果：顺序仍由 `updated_at` 决定，本次不做排序。
   */
  const dropOutOfGroup = () => {
    const ids = dragPayload;
    const originId = dragOrigin?.id;
    clearDrag();
    if (!originId) return;
    const current = groupOf(originId);
    if (!current) return;
    const drop = new Set((ids ?? [originId]).filter((id) => current.group.threadIds.includes(id)));
    if (drop.size === 0) return;
    void onSetGroupMembers(
      current.workspace,
      current.group.id,
      current.group.threadIds.filter((x) => !drop.has(x)),
    );
  };

  /**
   * 多选区间的锚点顺序：与侧栏实际渲染顺序一致（组框成员摊平在框的位置上）。
   *
   * 只用于 Shift 区间选择，取的是**渲染序**而非服务端序——两者在框内相同，
   * 但框让行序与 `threads` 不再一一对应，用户看到的是哪一个就该按哪一个选。
   */
  /**
   * Cmd/Ctrl + 点击：把一条会话加进 / 移出选择集。
   *
   * 抽成函数是因为**两种行**都要用它——普通行与组长行。组长行原先没接这个
   * 分支，Cmd+点它会直接 `onSelect` 跳走：用户以为选中了三条，实际只记到
   * 两条（现场报的「选了三个、按钮显示 2」）。
   */
  /**
   * 选择集里是否包含这条会话。
   *
   * 空集时把**当前打开的那条**算作已选：用户眼里的"前面那条会话"就是它，
   * 第一次 Cmd+点别的会话时，那条也该一并进组——否则前台一条 + Cmd 点两条
   * 只会得到 2，与用户数出来的 3 对不上。
   *
   * 这个规则只影响**显示与首次成集**，不往 state 里写：一旦用户自己动过选择
   * （哪怕只点了一条），就以 `selected` 为准，前台那条也能被单独取消。
   */
  const isSelected = (id: string) =>
    selected.size === 0 ? id === currentId : selected.has(id);

  /**
   * 「当前有效选择集」：显式选择为空时，前台那条算一条。
   *
   * `isSelected` 的显示规则与它必须同源，否则会出现「看起来选中、拖起来却是空的一批」
   * ——`beginDragPayload` 曾直接拿 `isSelected` 判定、却取 `selected` 的原值，
   * 于是拖前台那条等于拖一个空数组，松手什么都不发生（用户报的「当前会话拖不动」）。
   */
  const effectiveSelection = (): string[] =>
    selected.size === 0 ? (currentId === null ? [] : [currentId]) : [...selected];

  function toggleSelected(id: string) {
    setSelected((prev) => {
      // 空集 + 点的不是前台那条 → 先把前台那条收进来，再切换点击的这条。
      // 这样"前台一条 + Cmd 点两条" = 3，且用户不必先补点一次前台。
      const next = new Set(prev);
      if (prev.size === 0 && currentId !== null && currentId !== id) next.add(currentId);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
    setSelectionAnchor(id);
  }

  const visibleThreadIds = useMemo(
    () =>
      groups.flatMap((g) =>
        layoutWorkspace(g).flatMap((row) =>
          row.kind === "thread" ? [row.thread.thread_id] : row.members.map((t) => t.thread_id),
        ),
      ),
    [groups],
  );

  /** thread_id → 它所在 workspace 的路径（用于建组时的同目录校验）。 */
  const workspaceOfThread = useCallback(
    (id: string): string | null => {
      for (const g of groups) {
        if (g.threads.some((t) => t.thread_id === id)) return g.workspace;
      }
      return null;
    },
    [groups],
  );

  /** 某个会话当前所在的全部组（多对多；"移出"取第一个，与 `groupByThread` 同序）。 */
  const groupsForThread = useCallback(
    (id: string) =>
      groups.flatMap((g) =>
        layoutWorkspace(g)
          .filter((row): row is Extract<typeof row, { kind: "group" }> => row.kind === "group")
          .filter((row) => row.members.some((t) => t.thread_id === id))
          .map((row) => ({ workspace: g.workspace, group: row.group })),
      ),
    [groups],
  );

  /**
   * 能把该会话加进去的组：同 workspace、且还不是它的成员。
   *
   * 渲染期一次性计算（不是 hook）：右键菜单一次只属于一条会话。
   */
  const joinableGroups = menu?.kind === "thread" ? joinableFor(menu.id) : [];

  function joinableFor(id: string) {
    const owner = workspaceOfThread(id);
    if (!owner) return [];
    const current = new Set(groupsForThread(id).map((g) => g.group.id));
    const ws = groups.find((g) => g.workspace === owner);
    return (ws?.groups ?? [])
      .filter((group) => !current.has(group.id))
      .map((group) => ({ workspace: owner, group }));
  }

  /**
   * 一个分组框：标题常显、描述只在展开时显示、成员缩进一档。
   *
   * 框是**纯视图**：成员仍随服务端的 `updated_at` 排序，框本身没有自己的排序
   * 状态（spec 明确不做手动排序），这样「记得住」靠的是位置稳定，而不是靠
   * 用户维护顺序。
   */
  const renderGroupBox = (
    workspace: string,
    group: SessionGroup,
    members: ThreadSummary[],
    indentLevel: 0 | 2,
  ) => {
    // 搜索期间强制展开（与目录折叠同一口径）：命中成员被折叠挡住等于搜不到。
    const expanded = searching || expandedGroups.has(group.id);
    const editing =
      editingGroup?.groupId === group.id && editingGroup.workspace === workspace
        ? editingGroup.field
        : null;
    const menuOpen = menu?.kind === "groupBox" && menu.groupId === group.id;
    const commitField = async (field: "name" | "description") => {
      const patch = { [field]: groupDraft } as { name?: string; description?: string };
      const ok = await onUpdateGroup(workspace, group.id, patch);
      if (ok) {
        setEditingGroup(null);
        setGroupDraft("");
      }
    };
    return (
      <div
        key={group.id}
        data-group-box={group.id}
        {...(groupDropTarget === group.id ? { "data-group-drop-active": "" } : {})}
        onPointerEnter={() => {
          // 只认「正在拖一条没在组里的会话」：其余情况（置顶重排、拖的是组内成员）
          // 框不是落点，亮起来等于骗人。
          if (!dragActive || dragOrigin?.kind !== "group" || dragPayload === null) return;
          setGroupDropTarget(group.id);
        }}
        onPointerLeave={() => setGroupDropTarget((cur) => (cur === group.id ? null : cur))}
        className={`mx-2 my-1 rounded-md border ${
          groupDropTarget === group.id
            ? "border-fg-subtle bg-raised/40 ring-1 ring-fg-subtle"
            : "border-line-strong"
        }`}
      >
        <div className="flex items-center gap-1 px-2 py-1">
          <button
            type="button"
            aria-label={`${expanded ? "折叠" : "展开"}分组 ${group.name}`}
            title={expanded ? "Collapse" : "Expand"}
            onClick={() =>
              setExpandedGroups((prev) => {
                const next = new Set(prev);
                if (next.has(group.id)) next.delete(group.id);
                else next.add(group.id);
                return next;
              })
            }
            className="shrink-0 px-0.5 text-fg-subtle hover:text-fg-muted"
          >
            {expanded ? "▾" : "▸"}
          </button>
          {editing === "name" ? (
            <input
              data-group-name-input=""
              autoFocus
              value={groupDraft}
              maxLength={GROUP_NAME_MAX}
              onChange={(e) => setGroupDraft(e.target.value)}
              onCompositionStart={ime.onCompositionStart}
              onCompositionEnd={ime.onCompositionEnd}
              onBlur={() => {
                ime.resetComposition();
                void commitField("name");
              }}
              onKeyDown={(e) => {
                if (e.key === "Enter" && !isImeEnter(e, ime.composing.current))
                  void commitField("name");
                if (e.key === "Escape") setEditingGroup(null);
              }}
              className="min-w-0 flex-1 rounded bg-surface px-1 py-0.5 text-sm text-fg outline-none"
            />
          ) : (
            <div
              className="min-w-0 flex-1 truncate text-sm font-medium text-fg-muted"
              title={group.name}
              onClick={() => {
                setEditingGroup({ workspace, groupId: group.id, field: "name" });
                setGroupDraft(group.name);
              }}
              onContextMenu={(e) => {
                e.preventDefault();
                setNewMenuOpen(false);
                setMenu({ kind: "groupBox", ws: workspace, groupId: group.id });
              }}
            >
              {group.name}
            </div>
          )}
          <span className="shrink-0 text-xs text-fg-faint" data-group-count="">
            {members.length}
          </span>
        </div>
        {expanded && (
          <div className="border-t border-line-strong/60 px-2 py-1">
            {editing === "description" ? (
              <textarea
                data-group-description-input=""
                autoFocus
                value={groupDraft}
                maxLength={GROUP_DESCRIPTION_MAX}
                rows={2}
                onChange={(e) => setGroupDraft(e.target.value)}
                onBlur={() => void commitField("description")}
                onKeyDown={(e) => {
                  if (e.key === "Escape") setEditingGroup(null);
                }}
                className="w-full rounded bg-surface px-1 py-0.5 text-xs text-fg-muted outline-none"
              />
            ) : (
              <div
                data-group-description=""
                className="line-clamp-2 cursor-text text-xs text-fg-subtle"
                onClick={() => {
                  setEditingGroup({ workspace, groupId: group.id, field: "description" });
                  setGroupDraft(group.description);
                }}
              >
                {group.description || "（点击添加描述）"}
              </div>
            )}
          </div>
        )}
        {expanded &&
          members.map((t) =>
            renderThread(t, { rowAttr: "group", indentLevel: indentLevel === 2 ? 2 : 1 }),
          )}
        <Popover open={menuOpen} onClose={closeMenus}>
          <div
            role="menu"
            data-group-menu=""
            className="absolute left-4 z-20 rounded-md border border-line-strong bg-raised py-1 shadow-xl"
          >
            <button
              type="button"
              role="menuitem"
              onClick={() => {
                closeMenus();
                void onDeleteGroup(workspace, group.id);
              }}
              className="block w-full px-3 py-1.5 text-left text-xs whitespace-nowrap text-fg hover:bg-raised"
            >
              删除组
            </button>
          </div>
        </Popover>
      </div>
    );
  };

  const renderThread = (
    t: ThreadSummary,
    opts: {
      rowAttr?: "group" | "pinned";
      dragIndex?: number;
      /** 行相对项目组的缩进档位：0=普通会话行（项目组直属），1=看板会话行（嵌在
       * 「看板会话」小节里），2=附属根小节里的会话行（嵌在缩进的附属根小节里）。
       * 默认 0，保持既有渲染逐像素不变。 */
      indentLevel?: 0 | 1 | 2;
    } = {},
  ) => {
    const active = t.thread_id === currentId;
    const st = statuses.get(t.thread_id) ?? "idle";
    const un = unread.get(t.thread_id);
    const isPinned = t.pinned ?? false;
    const dragging = opts.rowAttr === "pinned";
    // 整行可拖：手柄几乎没人发现，而拖拽是「把会话放进框」最直觉的表达。
    // 按着修饰键时不拖——那时用户点的是多选，不是拖拽（见 `modifierDown`）。
    const canDragIntoGroup = opts.rowAttr === "group";
    // 看板会话行多缩进一档（pl-7 vs pl-3）：折叠小节头是 ▸ 视觉起点，其下的
    // 会话若与直属行同一起点，就分不清谁属于谁。缩进让「小节头 → 卡片会话」的
    // 从属关系一眼可读。仅改左内边距，不影响拖拽/点击。
    // 缩进逐档递增：0→pl-3（直属）、1→pl-7（看板会话）、2→pl-11（附属根会话）。
    const indent =
      opts.indentLevel === 2 ? "pl-11" : opts.indentLevel === 1 ? "pl-7" : "pl-3";
    const dataAttr =
      opts.rowAttr === "pinned" ? { "data-pinned-row": "" } : { "data-group-row": "" };
    // 「移出分组」的落点只认会话行：拖动一批组内成员时，悬停到框外任意一行都能
    // 松手移出。置顶行不做落点——它自己的拖拽是重排，两套语义别混。
    const canAcceptGroupDrop = dragPayload !== null && !dragging;
    return (
      <div
        key={t.thread_id}
        {...dataAttr}
        data-thread-row={t.thread_id}
        // 指针拖拽：按下先只记起点，越过 4px 阈值才算拖（否则仍是点击）。
        onPointerDown={
          dragging
            ? (e) => onRowPointerDown(t.thread_id, "pinned", e)
            : canDragIntoGroup
              ? (e) => onRowPointerDown(t.thread_id, "group", e)
              : undefined
        }
        {...(canAcceptGroupDrop && dropOverThread === t.thread_id
          ? { "data-group-drop-target": t.thread_id }
          : {})}
        onPointerEnter={
          // 置顶行：重排落点。其它行：移出分组的落点。两者互斥。
          dragging
            ? () => {
                if (!dragActive) return;
                setDropIndex(opts.dragIndex ?? null);
              }
            : canAcceptGroupDrop
              ? () => {
                  // 拖着的东西压根不在任何组里时，哪儿都不是「移出」落点，
                  // 否则整片侧栏都会亮起来而松手无事发生。
                  if (!dragOrigin || !groupOf(dragOrigin.id)) return;
                  setDropOverThread(t.thread_id);
                }
              : undefined
        }
        onPointerLeave={
          canAcceptGroupDrop
            ? () => setDropOverThread((cur) => (cur === t.thread_id ? null : cur))
            : undefined
        }
        onClick={(e) => {
          if (editingId) return;
          // 多选：Ctrl/Cmd 切换单选，Shift 按当前行序区间选择。没有修饰键时
          // 照旧打开会话——选择态只是「临时搭建的动作上下文」，不该改变单击的含义。
          //
          // 双击保护必须放在修饰键分支**之后**：macOS 上连续两次 Cmd+点击的
          // 第二次 `detail` 是 2（浏览器按连击计数，修饰键不参与判定），放在前面
          // 会把那次加选直接吞掉——多选两条、最后只进去一条。
          if (e.metaKey || e.ctrlKey) {
            e.preventDefault();
            toggleSelected(t.thread_id);
            return;
          }
          if (e.shiftKey && selectionAnchor) {
            e.preventDefault();
            const from = visibleThreadIds.indexOf(selectionAnchor);
            const to = visibleThreadIds.indexOf(t.thread_id);
            if (from >= 0 && to >= 0) {
              const [lo, hi] = from < to ? [from, to] : [to, from];
              setSelected(new Set(visibleThreadIds.slice(lo, hi + 1)));
            }
            return;
          }
          // 双击会先派发两次 click 再派发 dblclick;忽略第二次 click,
          // 避免对同一 thread 触发两次 onSelect(即两次 thread/resume)。
          // 只拦无修饰键的普通点击：多选/区间选择在上面的分支里已经 return。
          if (e.detail > 1) return;
          if (selected.size > 0) {
            // 有选择时点普通行 = 退出多选，而不是跳转——否则一次误点就把
            // 辛苦攒起来的选择清空并切走了会话。
            setSelected(new Set());
            setSelectionAnchor(null);
            return;
          }
          onSelect(t.thread_id);
        }}
        onContextMenu={(e) => {
          // 桌面端打标签的入口：右键会话行打开与手机长按**同一份**菜单。
          e.preventDefault();
          if (editingId === t.thread_id) return;
          setNewMenuOpen(false);
          setMenu({ kind: "thread", id: t.thread_id });
        }}
        tabIndex={0}
        aria-haspopup="menu"
        aria-expanded={menu?.kind === "thread" && menu.id === t.thread_id}
        onKeyDown={(e) => {
          // 键盘等价于右键：Enter/Space 开关本行的菜单。
          if (e.target !== e.currentTarget) return;
          if (e.key !== "Enter" && e.key !== " ") return;
          e.preventDefault();
          setNewMenuOpen(false);
          setMenu((cur) =>
            cur?.kind === "thread" && cur.id === t.thread_id
              ? null
              : { kind: "thread", id: t.thread_id },
          );
        }}
        className={`group relative flex items-center justify-between gap-1 ${indent} py-2 text-sm ${
          dragId === t.thread_id ? "opacity-50" : ""
        } ${isSelected(t.thread_id) ? "bg-raised ring-1 ring-line-strong ring-inset" : ""} ${
          dropOverThread === t.thread_id ? "ring-1 ring-amber-400 ring-inset" : ""
        } ${
          active ? "bg-raised text-fg" : "text-fg-muted hover:bg-raised/50"
        } cursor-pointer`}
      >
        {editingId === t.thread_id ? (
          <input
            data-rename-input=""
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
                <StatusBadge status={st} stale={statusStale} />
                {isValidTagId(t.tag) && (
                  <span
                    data-thread-tag={t.tag}
                    aria-label={tagLabel(t.tag, tagNames)}
                    title={tagLabel(t.tag, tagNames)}
                    className={`size-2 shrink-0 rounded-full ${TAG_DOT_CLASS[t.tag]}`}
                  />
                )}
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
              <div className="text-xs text-fg-faint">{sidebarAge(t.updated_at)}</div>
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
        {/*
          三个菜单互斥（见 `menu` 联合类型的注释），任何时刻至多一个 Popover 生效，
          故 Escape 的关闭条件与原手写 effect 完全一致。

          `data-thread-menu-overlay` 是 useLongPress 吞 click 的白名单、也是测试的
          选择器；`backdropProps` 最后展开，故这里必须自己给 onClick——且要先
          stopPropagation，否则行自身的 onClick 会顺手把这次点击当成选中会话。
        */}
        <Popover
          open={menu?.kind === "thread" && menu.id === t.thread_id}
          onClose={closeMenus}
          backdropProps={{
            "data-thread-menu-overlay": "",
            onClick: (e) => {
              e.stopPropagation();
              closeMenus();
            },
          }}
        >
          <div
            role="menu"
            data-thread-menu=""
            onClick={(e) => e.stopPropagation()}
            className="absolute top-full left-3 z-20 mt-0.5 min-w-32 rounded-md border border-line-strong bg-raised py-1 shadow-xl"
          >
            <div
              role="group"
              aria-label="标签"
              className="flex items-center gap-1.5 border-b border-line px-2 py-1.5"
            >
              {TAG_IDS.map((id) => {
                const active = t.tag === id;
                const name = tagLabel(id, tagNames);
                return (
                  <button
                    key={id}
                    type="button"
                    role="menuitemradio"
                    aria-checked={active}
                    aria-label={name}
                    title={name}
                    onClick={() => {
                      closeMenus();
                      onSetTag(t.thread_id, id);
                    }}
                    className={`size-3.5 rounded-full ${TAG_DOT_CLASS[id]} ${
                      active ? "ring-2 ring-fg ring-offset-1 ring-offset-raised" : ""
                    }`}
                  />
                );
              })}
              <button
                type="button"
                role="menuitemradio"
                aria-checked={!isValidTagId(t.tag)}
                onClick={() => {
                  closeMenus();
                  onSetTag(t.thread_id, null);
                }}
                className="ml-auto px-1 text-xs whitespace-nowrap text-fg-muted hover:text-fg"
              >
                无标签
              </button>
            </div>
            {groupsForThread(t.thread_id).length > 0 && (
              <button
                type="button"
                role="menuitem"
                data-leave-group=""
                onClick={() => {
                  closeMenus();
                  const current = groupsForThread(t.thread_id)[0];
                  void onSetGroupMembers(
                    current.workspace,
                    current.group.id,
                    current.group.threadIds.filter((id) => id !== t.thread_id),
                  );
                }}
                className="block w-full px-3 py-1.5 text-left text-xs whitespace-nowrap text-fg hover:bg-raised"
              >
                移出分组（{groupsForThread(t.thread_id)[0].group.name}）
              </button>
            )}
            {joinableGroups.length > 0 && (
              <div role="group" aria-label="加入分组" className="border-b border-line pb-1">
                {joinableGroups.map(({ workspace, group }) => (
                  <button
                    key={group.id}
                    type="button"
                    role="menuitem"
                    data-join-group={group.id}
                    onClick={() => {
                      closeMenus();
                      void onSetGroupMembers(workspace, group.id, [...group.threadIds, t.thread_id]);
                    }}
                    className="block w-full px-3 py-1.5 text-left text-xs whitespace-nowrap text-fg hover:bg-raised"
                  >
                    加入 {group.name}
                  </button>
                ))}
              </div>
            )}
            <button
              type="button"
              role="menuitem"
              onClick={() => {
                closeMenus();
                setEditingId(t.thread_id);
                setDraft(t.title ?? "");
              }}
              className="block w-full px-3 py-1.5 text-left text-xs whitespace-nowrap text-fg hover:bg-raised"
            >
              重命名
            </button>
            <button
              type="button"
              role="menuitem"
              onClick={() => {
                closeMenus();
                onTogglePin(t.thread_id, !isPinned);
              }}
              className="block w-full px-3 py-1.5 text-left text-xs whitespace-nowrap text-fg hover:bg-raised"
            >
              {isPinned ? "取消置顶" : "置顶"}
            </button>
            <button
              type="button"
              role="menuitem"
              onClick={() => {
                closeMenus();
                onDelete(t.thread_id);
              }}
              className="block w-full px-3 py-1.5 text-left text-xs whitespace-nowrap text-fg hover:bg-raised"
            >
              删除
            </button>
          </div>
        </Popover>
      </div>
    );
  };

  const renderBoardEntry = (workspace: string) => {
    const active = selectedBoard === workspace;
    return (
      <div
        key={`board:${workspace}`}
        aria-label="看板"
        title={workspace}
        onClick={(e) => {
          if (e.detail > 1) return;
          onOpenBoard(workspace);
        }}
        className={`flex cursor-pointer items-center gap-2 px-3 py-2 text-sm ${
          active ? "bg-raised text-fg" : "text-fg-muted hover:bg-raised/50"
        }`}
      >
        <span className="shrink-0">看板</span>
        {/* 摘要是这一眼要看的全部：排队几件、在跑几件。 */}
        {boardSummaries[workspace] ? (
          <span className="truncate text-xs text-fg-subtle">{boardSummaries[workspace]}</span>
        ) : null}
      </div>
    );
  };

  /**
   * 项目组内的「看板会话」小节：只收 `driver_ref` 非空的会话，默认折叠。
   * 折叠状态独立于项目组（键 `board:${workspace}`），互不牵连。
   *
   * 刻意不依赖看板是否仍登记：看板被移除后（spec §5），卡片会话仍须留在这个
   * 小节里可见，绝不能因为 `kanbanItemFor` 变 null 就整批消失。
   */
  const renderBoardThreads = (g: WorkspaceGroup) => {
    if (!kanbanEnabled) return null;
    const cards = g.threads.filter((t) => !t.pinned && t.driver_ref && t.thread_id !== leadId);
    if (cards.length === 0) return null;
    // 搜索期间一律展开，且**关掉这一小节自身的折叠开关**：此时渲染忽略折叠态，
    // 点了看不到任何反馈，却会悄悄写进 `boardCollapsed`，清空搜索后视图就被改掉了。
    const isBoardCollapsed = searching ? false : !boardCollapsed.has(g.workspace);
    return (
      <div key={`board-threads:${g.workspace}`}>
        <button
          type="button"
          aria-label="看板会话"
          aria-expanded={!isBoardCollapsed}
          disabled={searching}
          onClick={() => toggleBoardCollapse(g.workspace)}
          className="flex w-full items-center gap-1 px-2 py-1 text-xs text-fg-subtle hover:bg-raised/50 focus:bg-raised/50 focus:outline-none"
        >
          <span className="shrink-0 px-0.5">{isBoardCollapsed ? "▸" : "▾"}</span>
          <span className="min-w-0 flex-1 truncate text-left">看板会话 ({cards.length})</span>
        </button>
        {!isBoardCollapsed && cards.map((t) => renderThread(t, { rowAttr: "group", indentLevel: 1 }))}
      </div>
    );
  };

  // 组长条目：该项目若有 `role === "lead"` 的会话，它固定渲染在列表最顶部
  // （Pinned 分区之上），并且**不再**作为普通分组行出现——同一条会话出现两次
  // 会让人以为是两个入口。判定规则与服务端 `ThreadStore::find_lead` 一致
  // （`role === "lead"` 且 `created_at` 最小），未知 role 一律忽略。
  const lead = findLeadThread(groups.flatMap((g) => g.threads));
  // 开关关掉时把 `leadId` 一并置空，**不只是**不渲染那一行：`leadId` 还用于
  // 从普通分组里剔除组长会话（同一条会话不能出现两次）。只藏不剔的话，组长
  // 会话会从两处都消失——用户看到的是"我的会话不见了"。
  const leadId = leadAgentEnabled ? (lead?.thread_id ?? null) : null;

  // 组长行：与普通会话行同构（同样的状态徽标、标题、相对时间、未读点、点击选
  // 中），只多一个 `data-lead-row` 标记并去掉图钉/删除按钮——组长是项目级的固
  // 定入口，置顶与删除对它没有意义。仍带 `data-thread-row`（选中逻辑与样式
  // 规则要用），但 `resolveSidebarPressTarget` 会因 `data-lead-row` 先返回 null，
  // 故手机长按组长行**不会**弹出会话菜单。
  const renderLeadRow = (t: ThreadSummary) => {
    const active = t.thread_id === currentId;
    const st = statuses.get(t.thread_id) ?? "idle";
    const un = unread.get(t.thread_id);
    return (
      <div
        key={t.thread_id}
        data-lead-row=""
        data-thread-row={t.thread_id}
        onClick={(e) => {
          if (editingId) return;
          // 与普通会话行同一套多选语义：多选是"临时搭建的动作上下文"，
          // 组长行没有理由例外——它也是一条会话。
          if (e.metaKey || e.ctrlKey) {
            e.preventDefault();
            toggleSelected(t.thread_id);
            return;
          }
          if (e.detail > 1) return;
          if (selected.size > 0) {
            setSelected(new Set());
            setSelectionAnchor(null);
            return;
          }
          onSelect(t.thread_id);
        }}
        className={`group relative flex items-center justify-between gap-1 pl-3 py-2 text-sm ${
          active ? "bg-raised text-fg" : "text-fg-muted hover:bg-raised/50"
        } cursor-pointer`}
      >
        <LeadAvatar />
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-1.5">
            <StatusBadge status={st} stale={statusStale} />
            <div className="truncate" title={t.title ?? t.thread_id}>
              {t.title ?? "(untitled)"}
            </div>
          </div>
          <div className="text-xs text-fg-faint">{sidebarAge(t.updated_at)}</div>
        </div>
        {un && !active && (
          <span
            aria-label="Unread"
            title="Unread result"
            className={`size-2 shrink-0 rounded-full ${unreadDotClass(un)}`}
          />
        )}
      </div>
    );
  };

  // 渲染时过滤：不持有第二份列表副本，服务端刷新后自动跟随。
  const searching = query.trim() !== "";
  const filtered = filterThreadTree(groups, pinned, query, tagNames);
  // 看板关着时，卡片会话（`driver_ref` 非空）在哪儿都不该出现——置顶区是一处
  // 独立的分流，`filterThreadTree` 不会替我们滤掉它。组内那几处已有各自的
  // `!row.thread.driver_ref` / `renderBoardThreads` 守卫。
  const visiblePinned = kanbanEnabled
    ? filtered.pinned
    : filtered.pinned.filter((t) => !t.driver_ref);
  // 过滤之后再按 linked_to 归拢：搜索 / 置顶 / 看板会话三个既有分支都不受影响。
  // 搜索期间不复用归拢结果——`filterThreadTree` 只留下命中的组，附属根会临时回到
  // 顶层；渲染时用下面的 isLinkedChild 兜住"顶层但其实是附属根"这一情形。
  const nested = useMemo(
    () => nestLinkedGroups(filtered.groups, workspaces),
    [filtered.groups, workspaces],
  );
  const linkedChildOf = useMemo(() => {
    const m = new Map<string, string>();
    for (const w of workspaces) if (w.linked_to) m.set(w.path, w.linked_to);
    return m;
  }, [workspaces]);
  // 搜索期间忽略折叠状态；三个 Set 全程不被写入，清空后原样恢复。
  const pinnedCollapsedNow = searching ? false : pinnedCollapsed;

  // 组长行与搜索结果同口径：复用 `filterThreadTree` 的匹配规则（同一份
  // haystack：标题 + 目录名 + 完整路径），避免出现「搜到了组内会话、组长却
  // 凭空消失」或反之的分歧。无查询时 `filterThreadTree` 原样返回，恒可见。
  const leadVisible =
    leadAgentEnabled &&
    lead !== null &&
    filterThreadTree(
      [{ workspace: lead.cwd, exists: true, threads: [lead] }],
      [],
      query,
    ).groups.length > 0;

  const clearQuery = () => setQuery("");

  /**
   * 渲染一个目录分组节点。`indent === 0` 是主根（顶层组头，功能齐全）；`indent === 1`
   * 是附属根小节（缩进一档，菜单只有「取消关联」）。
   *
   * 两种节点共用这一份渲染，差异只在「入口开关」与缩进：附属根不能作会话 `cwd`
   * （cwd 决定 agent base prompt / store 定位 / skills 发现 / 权限相对路径），所以
   * 不提供「New thread here」；看板是**主根级**项目特性，附属根小节只承担"会话在
   * 这里"这一件事。若把两套菜单各写一遍，下次加菜单项时两边口径必然悄悄漂移。
   *
   * 搜索期间也不复用归拢结果：`filterThreadTree` 只留下命中的组，附属根会临时落到
   * 顶层——`linkedChildOf` 兜住这种"顶层但其实是附属根"的情形，菜单口径保持一致。
   */
  const renderGroupNode = (g: WorkspaceGroup, children: WorkspaceGroup[], indent: 0 | 1) => {
    const isChild = indent === 1 || linkedChildOf.has(g.workspace);
    // 搜索期间忽略折叠（与置顶分区同一口径）：否则命中附属根的会话行会被折叠状态
    // 挡住，搜到了却看不见。
    const isCollapsed = searching ? false : collapsed.has(g.workspace);
    const menuKind: "group" | "child" = isChild ? "child" : "group";
    const menuOpen = menu?.kind === menuKind && menu.ws === g.workspace;
    return (
      <div key={g.workspace} className={isChild ? "relative pl-3" : "relative"}>
        <div
          tabIndex={0}
          {...(isChild
            ? { "data-linked-child": g.workspace }
            : { "data-ws-header": g.workspace })}
          aria-haspopup="menu"
          aria-expanded={menuOpen}
          onContextMenu={(e) => {
            e.preventDefault();
            setNewMenuOpen(false);
            setMenu({ kind: menuKind, ws: g.workspace });
          }}
          /*
            目录标题不是「移出分组」的落点：它不是会话，把它当落点等于让用户猜。
            移出只认会话行（见 `renderThread` 的 `canAcceptGroupDrop`）。
          */
          onKeyDown={(e) => {
            // 键盘等价于右键:Enter/Space 打开(或关闭)该组的菜单。
            // 只处理组头自身获焦的情况——caret 按钮是子元素,其 keydown 会冒泡到
            // 此处;若在此 preventDefault 会吞掉 caret 的 Enter/Space 激活,
            // 导致键盘无法折叠分组(回归)。
            if (e.target !== e.currentTarget) return;
            if (e.key !== "Enter" && e.key !== " ") return;
            e.preventDefault();
            setNewMenuOpen(false);
            setMenu((cur) =>
              cur?.kind === menuKind && cur.ws === g.workspace
                ? null
                : { kind: menuKind, ws: g.workspace },
            );
          }}
          className={
            isChild
              ? "flex items-center gap-1 px-2 py-1 text-base font-medium text-fg-subtle hover:bg-raised/50 focus:bg-raised/50 focus:outline-none"
              : "flex items-center gap-1 px-2 py-1.5 text-base font-medium text-fg-subtle hover:bg-raised/50 focus:bg-raised/50 focus:outline-none"
          }
        >
          <button
            type="button"
            onClick={() => toggleCollapse(g.workspace)}
            disabled={searching}
            aria-label={isCollapsed ? "Expand" : "Collapse"}
            title={isCollapsed ? "Expand" : "Collapse"}
            className="shrink-0 px-0.5 text-fg-subtle hover:text-fg-muted"
          >
            {isCollapsed ? "▸" : "▾"}
          </button>
          <div
            className={`min-w-0 flex-1 truncate ${
              isChild ? "text-fg-faint" : g.exists ? "text-fg-muted" : "text-fg-faint"
            }`}
            title={g.workspace}
          >
            {basename(g.workspace)}
            {!g.exists && " (missing)"}
          </div>
        </div>
        <Popover open={menuOpen} onClose={closeMenus}>
          <div
            role="menu"
            className={`absolute ${
              isChild ? "top-6" : "top-7"
            } left-4 z-20 rounded-md border border-line-strong bg-raised py-1 shadow-xl`}
          >
            {!isChild && (
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
            )}
            {!isChild && kanbanEnabled && (
              <button
                type="button"
                role="menuitem"
                onClick={() => {
                  closeMenus();
                  if (kanbanItemFor(g.workspace, boards)) onRemoveBoard(g.workspace);
                  else onCreateBoard(g.workspace);
                }}
                className="block w-full px-3 py-1.5 text-left text-xs whitespace-nowrap text-fg hover:bg-raised"
              >
                {kanbanItemFor(g.workspace, boards) ? "移除看板" : "创建 Superpowers 看板"}
              </button>
            )}
            {isChild ? (
              <button
                type="button"
                role="menuitem"
                onClick={() => {
                  closeMenus();
                  onUnlinkFolder(g.workspace);
                }}
                className="block w-full px-3 py-1.5 text-left text-xs whitespace-nowrap text-fg hover:bg-raised"
              >
                取消关联
              </button>
            ) : (
              <>
                <button
                  type="button"
                  role="menuitem"
                  onClick={() => {
                    closeMenus();
                    onLinkFolder(g.workspace);
                  }}
                  className="block w-full px-3 py-1.5 text-left text-xs whitespace-nowrap text-fg hover:bg-raised"
                >
                  关联文件夹…
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
              </>
            )}
          </div>
        </Popover>
        {!isCollapsed &&
          !isChild &&
          !searching &&
          kanbanEnabled &&
          kanbanItemFor(g.workspace, boards) &&
          renderBoardEntry(g.workspace)}
        {!isCollapsed && renderBoardThreads(g)}
        {!isCollapsed &&
          layoutWorkspace(g)
            // 组长行单独渲染在列表最顶部（见 renderLeadRow），组内不再重复一条。
            // `!pinned` 只作用于**框外**的行：置顶的组内会话仍留在框里（spec 刻意例外），
            // 于是同一个会话可以在顶部 Pinned 区与框内各出现一次。
            .filter((row) =>
              row.kind === "thread"
                ? !row.thread.pinned &&
                  !row.thread.driver_ref &&
                  row.thread.thread_id !== leadId
                : true,
            )
            .map((row) =>
              row.kind === "thread"
                ? renderThread(row.thread, { rowAttr: "group", indentLevel: isChild ? 2 : 0 })
                : renderGroupBox(g.workspace, row.group, row.members, isChild ? 2 : 0),
            )}
        {children.map((child) => renderGroupNode(child, [], 1))}
      </div>
    );
  };

  /** 提交新建组；失败时保留输入态，让用户改完再试。 */
  const submitNewGroup = async () => {
    if (!newGroup) return;
    const id = await onCreateGroup(
      newGroup.workspace,
      newGroup.name,
      newGroup.description,
      [...selected],
    );
    if (id) {
      // 建完立刻展开新框：折叠态下只看得到「▸ 组名 2」，用户无从确认
      // 选中的每一条都进去了。
      setExpandedGroups((prev) => new Set(prev).add(id));
      setNewGroup(null);
      setSelected(new Set());
      setSelectionAnchor(null);
    }
  };

  /**
   * 多选操作条：只在有选择时出现。
   *
   * 建组要求所选会话同属一个 workspace——组不跨目录是硬约束（spec），在客户端
   * 先拦一道能给出可读提示，而不是让服务端回一个 -32602。
   */
  const selectionBar = selected.size === 0 ? null : (
    <div
      data-selection-bar=""
      className="sticky bottom-0 border-t border-line-strong bg-surface px-2 py-2"
    >
      {newGroup ? (
        <div className="flex flex-col gap-1">
          <input
            data-new-group-name=""
            autoFocus
            placeholder="组名"
            maxLength={GROUP_NAME_MAX}
            value={newGroup.name}
            onChange={(e) => setNewGroup({ ...newGroup, name: e.target.value })}
            onCompositionStart={ime.onCompositionStart}
            onCompositionEnd={ime.onCompositionEnd}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !isImeEnter(e, ime.composing.current)) {
                void submitNewGroup();
              }
              if (e.key === "Escape") setNewGroup(null);
            }}
            className="w-full rounded-md border border-line bg-surface px-2 py-1.5 text-sm text-fg placeholder:text-fg-faint outline-none focus:border-fg-subtle"
          />
          <input
            data-new-group-description=""
            placeholder="描述（可选）"
            maxLength={GROUP_DESCRIPTION_MAX}
            value={newGroup.description}
            onChange={(e) => setNewGroup({ ...newGroup, description: e.target.value })}
            className="w-full rounded-md border border-line bg-surface px-2 py-1.5 text-xs text-fg-muted placeholder:text-fg-faint outline-none focus:border-fg-subtle"
          />
          <div className="flex gap-2">
            <button
              type="button"
              onClick={() => void submitNewGroup()}
              className="rounded-md bg-raised px-3 py-1 text-xs text-fg hover:bg-raised/80"
            >
              创建
            </button>
            <button
              type="button"
              onClick={() => setNewGroup(null)}
              className="rounded-md px-3 py-1 text-xs text-fg-subtle hover:text-fg"
            >
              取消
            </button>
          </div>
        </div>
      ) : (
        <button
          type="button"
          onClick={() => {
            const ids = [...selected];
            const owner = workspaceOfThread(ids[0]);
            if (!owner) return;
            if (ids.some((id) => workspaceOfThread(id) !== owner)) {
              onError("所选会话不在同一个 workspace，无法归于一组");
              setSelected(new Set());
              setSelectionAnchor(null);
              return;
            }
            setNewGroup({ workspace: owner, name: "", description: "" });
          }}
          className="w-full rounded-md bg-raised px-3 py-2 text-sm text-fg hover:bg-raised/80"
        >
          新建组（{selected.size}）
        </button>
      )}
    </div>
  );

  const searchBar = (
    <div className="relative px-2 pb-2">
      <input
        type="search"
        aria-label="搜索会话"
        placeholder="搜索会话…"
        value={query}
        onChange={(e) => setQuery(e.target.value)}
        onKeyDown={(e) => {
          // 清空是幂等动作，不做 IME 特判：输入法组合期按 Esc 取消候选也不会损坏状态。
          if (e.key === "Escape") clearQuery();
        }}
        className="w-full rounded-md border border-line bg-surface px-2 py-1.5 text-sm text-fg placeholder:text-fg-faint focus:border-fg-subtle focus:outline-none"
      />
      {query !== "" && (
        <button
          type="button"
          aria-label="清除搜索"
          title="清除搜索"
          onClick={clearQuery}
          className="absolute top-1/2 right-3.5 -translate-y-1/2 rounded px-1 text-fg-subtle hover:text-fg"
        >
          ×
        </button>
      )}
    </div>
  );

  return (
    <aside
      {...(isMobile ? { ...longPress, "data-sidebar-longpress": "" } : {})}
      className="relative flex shrink-0 flex-col border-r border-line bg-panel"
      style={{ width }}
    >
      <div className="relative p-2">
        <button
          type="button"
          aria-haspopup="menu"
          aria-expanded={newMenuOpen}
          onPointerDown={onNewThreadPointerDown}
          onKeyDown={onNewThreadKeyDown}
          onClick={onNewThreadClick}
          className="relative w-full overflow-hidden rounded-md bg-raised px-3 py-2 text-left text-sm text-fg hover:bg-raised"
        >
          {ripples.map((r) => (
            <span
              key={r.id}
              data-ripple
              aria-hidden="true"
              onAnimationEnd={() => removeRipple(r.id)}
              className="thread-ripple"
              style={{ left: r.x, top: r.y, width: r.size, height: r.size }}
            />
          ))}
          <span className="relative">+ New thread</span>
        </button>
        <Popover open={newMenuOpen} onClose={closeMenus}>
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
        </Popover>
      </div>
      {!isMobile && searchBar}
      <div className="flex-1 overflow-y-auto">
        {isMobile && searchBar}
        <div data-thread-list-content="" className="pr-4">
          {groupCount(groups) === 0 && !searching && (
            <p className="px-3 py-2 text-xs text-fg-faint">选择一个目录开始</p>
          )}
          {searching && filtered.empty && (
            <p className="px-3 py-2 text-xs text-fg-faint">无匹配会话</p>
          )}
          {leadVisible && lead && <div>{renderLeadRow(lead)}</div>}
          {visiblePinned.length > 0 && (
            <div>
              <div className="flex items-center gap-1 px-2 py-1.5 text-xs text-fg-subtle">
                <button
                  type="button"
                  onClick={() => setPinnedCollapsed((v) => !v)}
                  disabled={searching}
                  aria-label={pinnedCollapsedNow ? "Expand pinned" : "Collapse pinned"}
                  title={pinnedCollapsedNow ? "Expand" : "Collapse"}
                  className="shrink-0 px-0.5 text-fg-subtle hover:text-fg-muted"
                >
                  {pinnedCollapsedNow ? "▸" : "▾"}
                </button>
                <div className="min-w-0 flex-1 truncate text-fg-muted">Pinned</div>
              </div>
              {!pinnedCollapsedNow &&
                visiblePinned.map((t, i) => renderThread(t, { rowAttr: "pinned", dragIndex: i }))}
            </div>
          )}
          {/* 分组之间留一档空档：无间距时上一条会话与下一个文件夹头紧贴，多个文件夹
              连成一整列、看不出边界。`space-y-2` 只作用于兄弟之间，故首个分组之前
              不多出空白，也不动置顶分区与组之间既有的留白。
              辅助线画在**每个分组的上面**（`> * + *` 排除了第一个分组）：文件夹名升到
              text-base 之后，仅靠间距仍不够一眼分清"这条会话属于上面那个文件夹还是
              下面那个"，一条线把归属切断。用上边框而非 `divide-y`——后者画在下一个
              兄弟的**下边框**上，线会落在上一个分组的末尾、离文件夹头还有 8px（实测
              上边框宽度 0px），不是"文件夹上方那条线"。颜色用语义 token `border-line`，
              跟随主题。 */}
          <div className="space-y-2 [&>*+*]:border-t [&>*+*]:border-line">
            {nested.top.map(({ group: g, children }) => renderGroupNode(g, children, 0))}
          </div>
        </div>
      </div>
      {selectionBar}
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
        <ActiveSessionCount count={runningCount} stale={statusStale} />
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
