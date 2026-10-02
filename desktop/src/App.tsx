import { useCallback, useEffect, useRef, useState } from "react";
import { isRemoteClient } from "./lib/platform";
import { RpcClient } from "./lib/rpc";
import { ThreadStore } from "./lib/threadStore";
import { transportFactory } from "./transportFactory";
import { ChatView } from "./components/ChatView";
import { MessageInput } from "./components/MessageInput";
import { StatusBar } from "./components/StatusBar";
import { ApprovalDialog } from "./components/ApprovalDialog";
import { ApprovalBanner } from "./components/ApprovalBanner";
import { ThreadSidebar } from "./components/ThreadSidebar";
import { SubagentRail } from "./components/SubagentRail";
import { SubagentTrace } from "./components/SubagentTrace";
import { TitleBar } from "./components/TitleBar";
import type {
  AgentCancelPreviewResult,
  AgentChildrenListResult,
  AgentTraceSnapshotResult,
  ThreadStatus,
  ThreadSummary,
  TurnStatus,
  Workspace,
  WorkspaceGroup,
} from "./lib/protocol";
import { childrenOf, SubagentRailStore } from "./lib/subagents";
import { formatError } from "./lib/errorMessage";
import { SuperpowersKanbanView } from "./components/SuperpowersKanbanView";
import { SuperpowersKanbanCollapsedBar } from "./components/SuperpowersKanbanCollapsedBar";
import { SuperpowersKanbanSettings } from "./components/SuperpowersKanbanSettings";
import { SuperpowersKanbanEnqueue } from "./components/SuperpowersKanbanEnqueue";
import {
  type BoardCardDto,
  type SwitchSource,
  enqueueBoardCard,
  fetchBoard,
  pluginIsUnavailable,
  readBoardSwitch,
  setBoardSwitch,
} from "./lib/superpowersKanbanSwitch";
import {
  type BoardErrorKind,
  BOARD_SUMMARY_UNREADABLE,
  boardErrorKind,
  summarize,
} from "./lib/boardIndex";
import { createBoard, listBoards, removeBoard } from "./lib/superpowersKanbanBoards";
import { threadStartParams } from "./lib/threadStart";
import { setPermissionModeParams, type ThreadMode } from "./lib/threadPermissionMode";
import { renderHelp } from "./lib/slash";
import { estimateCost, formatCost } from "./lib/pricing";
import { applyTheme, parseTheme, readCachedTheme, type Theme } from "./lib/theme";
import { SettingsDialog } from "./components/SettingsDialog";

/**
 * 看板失败的四种说法。
 *
 * 三种可操作的状态各自点名下一步：没建看板就点「创建看板」，进程不可达就说
 * 明是连接问题而不是插件问题，插件没装才让用户去装。`other` 不猜原因。
 */
const BOARD_ERROR_TEXT: Record<BoardErrorKind, string> = {
  not_created: "该项目尚未创建看板",
  daemon_down: "看板进程不可达",
  plugin_missing: "插件未安装",
  other: "看板读写失败",
};

/**
 * Read the persisted permission mode for a thread from the `thread/listAll`
 * response. `thread/resume`/`thread/start` responses do not carry the mode, so it
 * is read back from the listing. Returns `null` when the thread is absent from
 * the listing (so callers never mistake "unknown" for "normal"); a present
 * thread with an omitted `permission_mode` (legacy) is treated as "normal".
 *
 * 置顶会话同时存在于顶层 `pinned` 与它自己的分组内（服务端为向后兼容不摘除），
 * 两个来源都要扫：任一侧缺失都不能让模式回读落空。
 */
function modeForThread(groups: WorkspaceGroup[], pinned: ThreadSummary[], id: string): ThreadMode | null {
  const t =
    pinned.find((th) => th.thread_id === id) ??
    groups.flatMap((g) => g.threads).find((th) => th.thread_id === id);
  return t ? (t.permission_mode ?? "normal") : null;
}

/**
 * `-32013` means `turn/interject` arrived with no turn running; `-32012` means
 * `turn/start` arrived while one was. Both say "the other method was the right
 * one" — the server resolves the disagreement, so the code is read from the raw
 * rejection (`formatError` would drop it).
 */
function sendMethodMismatchCode(e: unknown): number | null {
  const code = (e as { code?: unknown } | null)?.code;
  return code === -32013 || code === -32012 ? code : null;
}

export default function App() {
  // Per-thread state lives in a mutable store; the `force` tick is how React
  // learns that a view changed (the views are mutated in place, not setState'd).
  const storeRef = useRef(new ThreadStore());
  const store = storeRef.current;
  const [, force] = useState(0);
  const clientRef = useRef<RpcClient | null>(null);
  const inited = useRef(false);
  // Threads whose history has already been replayed (resumed) this session;
  // switching back to them must not replay again.
  const warm = useRef(new Set<string>());
  // Resume requests currently on the wire, so a rapid second click cannot
  // interleave two histories into one session.
  const inFlightResume = useRef(new Set<string>());
  // Approvals the user has dismissed from the banner; keyed by approval id.
  const dismissedApprovals = useRef(new Set<string>());
  const [currentId, setCurrentId] = useState<string | null>(null);
  const [status, setStatus] = useState<string>("connecting");
  const [groups, setGroups] = useState<WorkspaceGroup[]>([]);
  // 服务端排好序的全部置顶会话（从顶到底）。置顶项仍留在各自的分组内，侧栏
  // 渲染分组时需自行过滤，否则会重复渲染。
  const [pinned, setPinned] = useState<ThreadSummary[]>([]);
  const [workspaces, setWorkspaces] = useState<Workspace[]>([]);
  // 子 agent 暂留区:按对话隔离的折叠状态 + 用户是否收起了它。收起是纯 UI 选择,
  // 不进 store;数据本身仍随通知累积,展开即是当前值。
  const railStore = useRef(new SubagentRailStore()).current;
  const [railCollapsed, setRailCollapsed] = useState(false);
  // 打开的详情:进入栈 + 该栈顶任务的轨迹行。栈让"子任务再进入"可退回,
  // 与 TUI 页签同一语义。
  const [detailStack, setDetailStack] = useState<string[]>([]);
  const [traceRows, setTraceRows] = useState<AgentTraceSnapshotResult["rows"]>([]);
  const openSubagent = detailStack.length > 0 ? detailStack[detailStack.length - 1] : null;
  // 通知回调在 effect 里注册一次,读不到最新的 state,所以当前打开的任务放在 ref。
  const openTaskId = useRef<string | null>(null);
  useEffect(() => {
    openTaskId.current = openSubagent;
  }, [openSubagent]);
  const [boardOn, setBoardOn] = useState(false);
  const [boardSource, setBoardSource] = useState<SwitchSource>("default");
  const [boardCards, setBoardCards] = useState<BoardCardDto[]>([]);
  // The plugin answers every board question, so an unanswered query means it is
  // not installed. Distinct from "installed and empty".
  const [boardPluginMissing, setBoardPluginMissing] = useState(false);
  // 上一次看板写入的失败：非空即把「为什么没生效」摆在面板上，而不是静默。
  // 只有下一次写入成功或换了项目才清掉——轮询成功不算，那会让一条刚报出的
  // 失败在 2 秒内自己消失。
  const [boardError, setBoardError] = useState<BoardErrorKind | null>(null);

  /**
   * 一行看板状态，2 秒刷新一次。
   *
   * 用 ref 指向「最近一次的刷新函数」：副作用只在挂载时注册一次计时器，换项目
   * 时换掉里面调的闭包，而不是重建计时器（重建会漏掉切换瞬间的竞态，也会让
   * 两个项目的响应互相覆盖）。
   */
  const boardTick = useRef<() => void>(() => {});
  /** 看板读取的序号，用来丢掉换项目后落地的过期响应。 */
  const boardSeq = useRef(0);
  const [settingsOpen, setSettingsOpen] = useState(false);
  // 首屏先用缓存渲染，连上后以 ui/settings/read 的权威值为准。
  const [theme, setTheme] = useState<Theme>(() => readCachedTheme() ?? "dark");
  // 挂载后主题是否已被更新的选择触碰过（ui/settings/updated 通知或用户
  // changeTheme）。首屏 read 在途时若被触碰，read 返回的旧值不得覆盖它。
  const themeTouchedRef = useRef(false);

  /** 经 app-server 调 board RPC；未连接时直接失败。 */
  const boardRpc = useCallback(
    <T = unknown,>(method: string, params: unknown): Promise<T> => {
      const c = clientRef.current;
      if (!c) return Promise.reject(new Error("not connected"));
      return c.request<T>(method, params);
    },
    [],
  );

  // 看板 RPC 按项目问话（`project` 进 plugin/query 的参数）。当前在主区域
  // 展示看板的项目；与 currentId 相互独立——看会话不动看板，看板也不动会话。
  const [selectedBoard, setSelectedBoard] = useState<string | null>(null);
  // 看板可以「收起」而不「关闭」：收起后轮询与状态照旧，只是不画面板，
  // 并在原处留一条可展开的横条。收起是纯 UI 选择，不进 store。
  const [boardCollapsed, setBoardCollapsed] = useState(false);
  // 登记了看板的项目（侧栏据此画条目 + 决定右键菜单给创建还是移除）。
  const [boards, setBoards] = useState<string[]>([]);
  // 项目路径 → 摘要。挂在侧栏条目上，扫一眼就知道各项目积压多少。
  const [boardSummaries, setBoardSummaries] = useState<Record<string, string>>({});

  /**
   * 拉一次「哪些项目有看板」以及各项目的摘要。
   *
   * 摘要逐个项目单独读、单独失败：一个项目的 daemon 没起来不该让整张表消失
   * （条目照画，摘要留空）。整张表读不到才当作没有看板——侧栏据此不画条目。
   */
  const refreshBoards = useCallback(async () => {
    try {
      const list = await listBoards(boardRpc);
      setBoards(list);
      const entries = await Promise.all(
        list.map(async (path): Promise<[string, string]> => {
          try {
            return [path, summarize(await fetchBoard(boardRpc, path))];
          } catch {
            // 摘要读不到就明说「无法读取」，别留空：留空和「空看板」长得一样，
            // 而这两件事对用户是两码事（一个要去看 daemon/插件，一个什么都不用做）。
            return [path, BOARD_SUMMARY_UNREADABLE];
          }
        }),
      );
      setBoardSummaries(Object.fromEntries(entries));
    } catch {
      setBoards([]);
      setBoardSummaries({});
    }
  }, [boardRpc]);

  /**
   * 读选中项目的看板。没选中项目就什么都不问，也把面板清空——否则切走之后
   * 旧项目的卡片会留在屏幕上，看着像新项目的。
   *
   * 读取失败不改主流程，也不报错：失败在这里是常态（daemon 没起、插件没装），
   * 真正需要用户知道的失败是**写入**失败。
   */
  const refreshSelectedBoard = useCallback(async () => {
    if (selectedBoard === null) {
      setBoardCards([]);
      setBoardPluginMissing(false);
      return;
    }
    // 每次读领一个号。切项目后旧项目那次读可能还在路上，落地时必须认出自己
    // 已经过期——否则 A 的卡片会盖在 B 的看板上，看着像 B 的队列。
    const seq = ++boardSeq.current;
    try {
      const [sw, cards] = await Promise.all([
        readBoardSwitch(boardRpc, selectedBoard),
        fetchBoard(boardRpc, selectedBoard),
      ]);
      if (seq !== boardSeq.current) return;
      setBoardOn(sw.on);
      setBoardSource(sw.source);
      setBoardCards(cards);
      setBoardPluginMissing(false);
    } catch (error) {
      if (seq !== boardSeq.current) return;
      // 插件不在就明说：那意味着看板根本没有后端，而不是「装好了但没有卡片」。
      if (pluginIsUnavailable(error)) setBoardPluginMissing(true);
    }
  }, [boardRpc, selectedBoard]);

  useEffect(() => {
    // 计时器只建一次，每次响都走「当前项目」的读法：重建计时器会漏掉切换
    // 瞬间在途的那一拍。
    boardTick.current = () => void refreshSelectedBoard();
    // 选中项目一落地就读一次，不等下一拍轮询：否则点开看板会有最多 2 秒的空面板。
    void refreshSelectedBoard();
  }, [refreshSelectedBoard]);

  const current = currentId ? store.view(currentId) : null;

  /** Write an error onto the current thread's session (if any). */
  const setCurrentError = (msg: string) => {
    const s = store.current()?.session;
    if (s) s.lastError = msg;
  };

  const refreshThreads = async (): Promise<WorkspaceGroup[] | null> => {
    const c = clientRef.current;
    if (!c) return null;
    try {
      const r = await c.request<{ groups: WorkspaceGroup[]; pinned?: ThreadSummary[] }>(
        "thread/listAll",
        {},
      );
      setGroups(r.groups);
      setPinned(r.pinned ?? []);
      // seed 喂全集：置顶项仍在分组里，所以不会漏；store 的视图与侧栏的分区
      // 划分无关（状态徽章、未读点都按 thread_id 查 store）。
      store.seed(r.groups.flatMap((g) => g.threads));
      return r.groups;
    } catch {
      // 列表刷新失败不打断对话。返回 null 表示"未知":调用方必须把 null
      // 当作未知处理,不得回退成 normal。注意权限模式不会因此自动恢复——
      // 它保持未知,直到下一次切换线程时重新从 listAll 回读。
      return null;
    }
  };

  const refreshWorkspaces = async () => {
    const c = clientRef.current;
    if (!c) return;
    try {
      const r = await c.request<{ workspaces: Workspace[] }>("workspace/list", {});
      setWorkspaces(r.workspaces);
    } catch {
      // 最近目录刷新失败不影响当前对话。
    }
  };

  /**
   * 打开某任务的详情:回灌历史,然后开一条流。
   *
   * 关闭(或换任务)时先 unwatch 再开新的,任何时刻只有一路流:否则换任务后
   * 旧任务的 `agent/trace/event` 仍会到达,把两个任务的轨迹混进一个视图。
   */
  const openDetail = async (threadId: string, taskId: string) => {
    const c = clientRef.current;
    if (!c) return;
    setDetailStack([taskId]);
    setTraceRows([]);
    try {
      const snapshot = await c.request<AgentTraceSnapshotResult>("agent/trace/read", {
        threadId,
        taskId,
      });
      setTraceRows(snapshot.rows);
      await c.request("agent/trace/watch", { threadId, taskId });
    } catch (e) {
      setCurrentError(formatError(e));
      force((v) => v + 1);
    }
  };

  const closeDetail = async (threadId: string) => {
    setDetailStack([]);
    setTraceRows([]);
    try {
      await clientRef.current?.request("agent/trace/unwatch", { threadId });
    } catch {
      // 关闭是本地动作:即使 unwatch 失败也不把详情留在屏幕上。
    }
  };

  /**
   * 取某对话当前的子 agent 列表。
   *
   * 失败即放弃:暂留区是附属视图,列表读不到就保持上一次的值(或空),绝不因为
   * 它把对话打断。daemon 未 attach 时服务端返回空表,这里不会走到 catch。
   */
  const refreshSubagents = async (threadId: string) => {
    const c = clientRef.current;
    if (!c) return;
    try {
      const r = await c.request<AgentChildrenListResult>("agent/children/list", {
        threadId,
      });
      railStore.set(threadId, r.children);
      force((v) => v + 1);
    } catch {
      // 附属视图:读失败不改动已有内容。
    }
  };

  /**
   * Switch the visible thread. A warm thread (already resumed) only swaps the
   * view — its timeline kept accumulating in the background. A cold thread is
   * resumed once; per-thread isolation means the base session list/sidebar can
   * keep serving other threads in parallel.
   */
  const selectThread = async (id: string) => {
    store.select(id);
    setCurrentId(id);
    force((v) => v + 1);
    // 该对话的子 agent 列表:重进对话时重新拉取,免得依赖"通知一定到过"。
    void refreshSubagents(id);
    if (warm.current.has(id) || inFlightResume.current.has(id)) return; // warm → 只切视图
    const c = clientRef.current;
    if (!c) return;
    inFlightResume.current.add(id);
    try {
      await c.request("thread/resume", { threadId: id });
      // 删除竞态:resume 在途时用户可能已删掉该冷 thread,此时不能再把它
      // 加回 warm(会复活已删 id)。peek 不创建视图,仅判断是否仍存在。
      const view = store.peek(id);
      if (!view) return;
      warm.current.add(id);
      // thread/resume 响应不带权限模式,从 listAll 回读后写回该 view。
      // 按 thread 存储,切回 warm thread 时 chip 自动反映各自模式。
      const gs = await refreshThreads();
      if (gs !== null) view.mode = modeForThread(gs, pinned, id);
      force((v) => v + 1);
    } catch (e) {
      // 同样地,失败路径只在视图仍存在时写错误,避免 re-create 一个已被
      // drop 的 ThreadView(无界泄漏 + 残留 warm 条目)。
      const view = store.peek(id);
      if (view) view.session.lastError = formatError(e);
      force((v) => v + 1);
    } finally {
      inFlightResume.current.delete(id);
    }
  };

  const newThread = async (cwd?: string) => {
    const c = clientRef.current;
    if (!c) return;
    try {
      const t = await c.request<{ thread_id: string; cwd: string; model: string }>(
        "thread/start",
        threadStartParams(cwd),
      );
      warm.current.add(t.thread_id);
      store.view(t.thread_id).info = { cwd: t.cwd, model: t.model };
      // 新对话默认 normal;仍从 listAll 回读以与服务端保持一致。
      store.view(t.thread_id).mode = "normal";
      store.select(t.thread_id);
      setCurrentId(t.thread_id);
      force((v) => v + 1);
      const gs = await refreshThreads();
      if (gs !== null) {
        store.view(t.thread_id).mode = modeForThread(gs, pinned, t.thread_id);
        force((v) => v + 1);
      }
    } catch (e) {
      setCurrentError(formatError(e));
      force((v) => v + 1);
    }
  };

  /**
   * 登记一个项目的看板并立刻刷新侧栏。
   *
   * 失败写进当前会话的错误位（与其它侧栏动作一致），刷新仍然执行：宿主可能
   * 已经建了一半，界面必须与它保持一致，而不是停在「看起来什么都没发生」。
   */
  const onCreateBoard = async (path: string) => {
    setBoardError(null);
    try {
      await createBoard(boardRpc, path);
    } catch (e) {
      setCurrentError(formatError(e));
      force((v) => v + 1);
    }
    await refreshBoards();
  };

  /**
   * 移除看板。决策 5 说这一下不可逆（队列状态会被删掉），所以先问一句；
   * 用户说不，就连 RPC 都不发。
   */
  const onRemoveBoard = async (path: string) => {
    if (!window.confirm(`移除看板会删除 ${path} 的队列状态，且不可恢复。继续？`)) return;
    setBoardError(null);
    try {
      await removeBoard(boardRpc, path);
    } catch (e) {
      setCurrentError(formatError(e));
      force((v) => v + 1);
    }
    // 正在看的看板被移除了：主区域不能再展示一个已经不存在的看板。
    if (selectedBoard === path) setSelectedBoard(null);
    await refreshBoards();
  };

  /** 打开看板只改 selectedBoard，不动 currentId。 */
  const onOpenBoard = (path: string) => {
    // 打开（或重新打开）一个看板一定展开它：从收起横条点进来、或换项目，都是
    // 「我现在要看这个看板」的意图，不该还停在收起的横条上。
    if (path === selectedBoard) {
      setBoardCollapsed(false);
      return;
    }
    // 换项目等于换问题：上一个项目的失败说法和卡片留在屏幕上只会误导。
    setBoardError(null);
    setBoardCards([]);
    setSelectedBoard(path);
    setBoardCollapsed(false);
  };

  /**
   * 写开关。失败不再吞掉：把失败翻译成三种可操作的说法摆出来。
   *
   * 成功才更新本地状态（服务端是权威），失败保持原状并报出原因——原来那句
   * `.catch(() => {})` 正是「点了没反应」的来源。
   */
  const onToggleBoardSwitch = async (next: boolean) => {
    if (selectedBoard === null) return;
    setBoardError(null);
    try {
      await setBoardSwitch(boardRpc, selectedBoard, next);
      setBoardOn(next);
      setBoardSource("project");
    } catch (error) {
      setBoardError(boardErrorKind(error));
    }
  };

  /** 打开原生文件夹选择器,返回选中的绝对路径(取消则 null)。 */
  const pickDirectory = async (): Promise<string | null> => {
    // 远程(iOS)构建没有 Tauri dialog 插件:不动态 import,直接当作取消。
    if (isRemoteClient()) return null;
    const { open } = await import("@tauri-apps/plugin-dialog");
    const picked = await open({ directory: true, multiple: false });
    return typeof picked === "string" ? picked : null;
  };

  /** 原生选择器选一个文件；取消返回 null。与 pickDirectory 同款动态 import。 */
  const pickFile = async (): Promise<string | null> => {
    if (isRemoteClient()) return null;
    const { open } = await import("@tauri-apps/plugin-dialog");
    const picked = await open({ directory: false, multiple: false });
    return typeof picked === "string" ? picked : null;
  };

  const addWorkspace = async (path: string): Promise<boolean> => {
    try {
      await clientRef.current?.request("workspace/add", { path });
      return true;
    } catch (e) {
      setCurrentError(formatError(e));
      force((v) => v + 1);
      return false;
    }
  };

  /** 打开原生选择器 → 加入最近目录 → 在该目录新建对话。 */
  const onBrowse = async () => {
    try {
      const dir = await pickDirectory();
      if (!dir) return;
      // add 失败(-32602 等)时不再建对话,错误已写入 lastError。
      if (!(await addWorkspace(dir))) return;
      await newThread(dir);
      await refreshWorkspaces();
    } catch (e) {
      setCurrentError(formatError(e));
      force((v) => v + 1);
    }
  };

  /** 侧栏入口:带 cwd 直接在该目录新建;无 cwd 时退化为弹原生选择器。 */
  const onNew = async (cwd?: string) => {
    if (cwd) {
      await newThread(cwd);
      // thread/start 会顺带把该目录写入最近目录索引。
      await refreshWorkspaces();
    } else {
      await onBrowse();
    }
  };

  const removeWorkspace = async (path: string) => {
    try {
      await clientRef.current?.request("workspace/remove", { path });
    } catch (e) {
      setCurrentError(formatError(e));
      force((v) => v + 1);
    }
    // 「移除」只是视图操作,不删数据:目录离开索引后 thread/listAll 不再扫描它,
    // 分组随之从侧栏消失,但 <dir>/.yi-agent/ 下的对话文件仍然保留。
    // 失败时也刷新,保证 UI 与服务端状态一致。
    await refreshThreads();
    await refreshWorkspaces();
  };

  const renameThread = async (id: string, title: string) => {
    const c = clientRef.current;
    if (!c) return;
    try {
      await c.request("thread/rename", { threadId: id, title });
    } catch (e) {
      setCurrentError(formatError(e));
      force((v) => v + 1);
    }
    await refreshThreads();
  };

  const deleteThread = async (id: string) => {
    const c = clientRef.current;
    if (!c) return;
    try {
      await c.request("thread/delete", { threadId: id });
    } catch (e) {
      setCurrentError(formatError(e));
      force((v) => v + 1);
      return;
    }
    // dismissedApprovals 以 approval id 为键;删除 thread 前先摘掉它名下那条
    // 审批,否则按 thread id delete 是 no-op 且会留下永不过期的条目。
    const v = store.peek(id);
    if (v?.approval) dismissedApprovals.current.delete(v.approval.id);
    store.drop(id);
    warm.current.delete(id);
    if (id === currentId) setCurrentId(null);
    force((v) => v + 1);
    await refreshThreads();
  };

  /** 置顶 / 取消置顶；成功后重拉列表，以服务端顺序为权威。 */
  const onTogglePin = async (id: string, next: boolean) => {
    const c = clientRef.current;
    if (!c) return;
    try {
      await c.request("thread/setPinned", { threadId: id, pinned: next });
      await refreshThreads();
      force((v) => v + 1);
    } catch (e) {
      setCurrentError(formatError(e));
      // 失败也要重拉：本地不做乐观更新，UI 不能停在"看起来置顶了"的状态。
      await refreshThreads();
      force((v) => v + 1);
    }
  };

  /** 置顶分区内拖拽排序落盘；乐观更新 + 失败回滚重拉。 */
  const onReorderPinned = async (orderedIds: string[]) => {
    const c = clientRef.current;
    if (!c) return;
    const prev = pinned;
    const byId = new Map(prev.map((t) => [t.thread_id, t]));
    // 乐观：先按落点顺序重排，`pin_seq` 的权威值仍由服务端在重拉时给出。
    setPinned(orderedIds.map((id) => byId.get(id)).filter((t): t is ThreadSummary => !!t));
    force((v) => v + 1);
    try {
      await c.request("thread/reorderPinned", { threadIds: orderedIds });
    } catch (e) {
      setCurrentError(formatError(e));
      setPinned(prev);
      await refreshThreads();
      force((v) => v + 1);
    }
  };

  /** 切换当前 thread 的权限模式;仅当 RPC 成功后才更新本地状态。 */
  const setThreadMode = async (next: ThreadMode) => {
    const c = clientRef.current;
    const id = store.currentId;
    if (!c || !id) return;
    try {
      await c.request("thread/setPermissionMode", setPermissionModeParams(id, next));
      store.view(id).mode = next;
      force((v) => v + 1);
    } catch (e) {
      setCurrentError(formatError(e));
      force((v) => v + 1);
    }
  };

  useEffect(() => {
    if (inited.current) return; // guard against React StrictMode double-invoke
    inited.current = true;
    const client = new RpcClient(transportFactory());
    clientRef.current = client;
    client.onNotification((n) => {
      // 子 agent 的通知不进 ThreadStore:它们是对话的附属视图,不是对话本身。
      if (n.method === "agent/children/updated") {
        railStore.applyNotification(n.params.threadId, n.params.children);
        force((v) => v + 1);
        return;
      }
      if (n.method === "ui/settings/updated") {
        // 对话（set_theme 工具）改了主题：跟随它。同时标记已触碰，免得仍在途
        // 的首屏 read 用旧值把它覆盖回去。
        themeTouchedRef.current = true;
        setTheme(parseTheme(n.params.theme));
        return;
      }
      if (n.method === "agent/trace/event") {
        // 只接受当前打开任务的流:换任务时旧流可能还有在途帧,丢弃它们比
        // 把两个任务的轨迹拼在一起安全。
        setTraceRows((prev) =>
          n.params.taskId === openTaskId.current ? [...prev, n.params.row] : prev,
        );
        return;
      }
      // 按 thread_id 路由:后台 thread 的流式输出照常累积,切回去即最新。
      store.applyNotification(n);
      force((v) => v + 1);
      if (n.method === "turn/completed") void refreshThreads();
    });
    client.onApproval((r) => {
      store.setApproval(r);
      force((v) => v + 1);
    });
    client.onStatus((s) => setStatus(s.state));
    (async () => {
      await client.request("initialize", {});
      const settings = await client.request<{ theme?: unknown }>("ui/settings/read", {});
      // 只有在此之后没有更新的主题选择时才采纳权威值：read 在途期间用户改了
      // 主题或收到 ui/settings/updated，更新的那个才是当前选择。
      if (!themeTouchedRef.current) setTheme(parseTheme(settings.theme));
      await refreshWorkspaces();
      const list = await client.request<{ groups: WorkspaceGroup[]; pinned?: ThreadSummary[] }>(
        "thread/listAll",
        {},
      );
      setGroups(list.groups);
      setPinned(list.pinned ?? []);
      store.seed(list.groups.flatMap((g) => g.threads));
      // 置顶分区在最上方，服务端给的顺序就是首屏该选中的第一个。
      const first = (list.pinned ?? [])[0] ?? list.groups.flatMap((g) => g.threads)[0];
      if (first) {
        await selectThread(first.thread_id);
      }
      // 否则保持空态,等用户选目录新建(设计 §7.2:不再自动在 $HOME 建对话)。
      setStatus("connected");
      // 看板登记表要等握手完成后再拉：`board/list` 是普通请求，服务端在
      // `initialize` 之前一律以 not_initialized 拒绝。早拉一次会被拒、把
      // boards 清空，而登记表只在这里拉一次，于是整场会话侧栏都没有看板条目。
      await refreshBoards();
    })().catch((e) => {
      const msg = formatError(e);
      setCurrentError(msg);
      setStatus(`error: ${msg}`);
    });
    // 看板内容由 boardTick 每 2 秒刷新选中项目，读失败绝不影响主流程。
    const boardTimer = window.setInterval(() => boardTick.current(), 2000);
    return () => window.clearInterval(boardTimer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  /**
   * Run a slash command. Commands never reach the agent: their output is a
   * `notice` on the current session, mirroring the TUI's Separator cells.
   */
  const onSlashCommand = async (name: string, args: string | null) => {
    const id = store.currentId;
    const c = clientRef.current;
    if (!id || !c) return;
    const view = store.view(id);
    const session = view.session;
    switch (name) {
      case "help":
        session.notice(renderHelp(args));
        break;
      case "cost": {
        const u = session.usage;
        if (!u) {
          session.notice("暂无用量数据");
          break;
        }
        const cost = estimateCost(u);
        session.notice(
          [
            `model: ${u.model}`,
            `input: ${u.input}  output: ${u.output}`,
            `cache read: ${u.cacheRead}  cache write: ${u.cacheWrite}`,
            `估算成本: ${formatCost(cost)}`,
          ].join("\n"),
        );
        break;
      }
      case "model":
        session.notice(`当前模型: ${view.info?.model ?? "未知"}`);
        break;
      case "config":
        try {
          const cfg = await c.request<Record<string, unknown>>("config/read", {});
          session.notice(
            Object.entries(cfg)
              .map(([k, v]) => `${k}: ${String(v)}`)
              .join("\n"),
          );
        } catch (e) {
          session.notice(`读取配置失败: ${formatError(e)}`);
        }
        break;
      case "clear":
        try {
          await c.request("thread/clear", { threadId: id });
          session.reset();
          session.notice("对话已清空");
        } catch (e) {
          // 失败绝不清界面:否则会出现"看着清了、服务端还记得"的假象。
          session.notice(`清空失败: ${formatError(e)}`);
        }
        break;
      case "compact":
        try {
          const r = await c.request<{ status?: string; error?: string }>("thread/compact", {
            threadId: id,
          });
          if (r.status === "compacted") session.notice("对话已压缩");
          else if (r.status === "not_reduced") session.notice("无需压缩：历史太短");
          else session.notice(`压缩失败: ${r.error ?? "未知结果"}`);
        } catch (e) {
          session.notice(`压缩失败: ${formatError(e)}`);
        }
        break;
      default:
        session.notice(`未知命令: /${name}\n${renderHelp()}`);
        break;
    }
    force((v) => v + 1);
  };

  const send = async (text: string): Promise<boolean> => {
    const id = store.currentId;
    const c = clientRef.current;
    if (!id || !c) return false;
    const session = store.view(id).session;
    session.addUserMessage(text);
    force((v) => v + 1);
    // The cached status can be stale: `turn/completed` reaches us before the
    // server flips the thread back to idle, and a listing read inside that
    // window keeps the old value. Send the method the cache implies, then let
    // the server's error code say where it disagreed and switch to the other
    // one. Each direction is tried once, so two mismatches cannot ping-pong.
    const params = { threadId: id, input: [{ type: "text", text }] };
    let method: "turn/interject" | "turn/start" =
      store.peek(id)?.status === "running" ? "turn/interject" : "turn/start";
    let lastError: unknown = null;
    for (let hop = 0; hop < 2; hop += 1) {
      try {
        await c.request(method, params);
        return true;
      } catch (e) {
        lastError = e;
        const code = sendMethodMismatchCode(e);
        if (code === null) break;
        // Adopt the server's view so the next send (and the button) is right.
        method = method === "turn/interject" ? "turn/start" : "turn/interject";
        session.turnActive = method === "turn/interject";
        const view = store.peek(id);
        if (view) view.status = method === "turn/interject" ? "running" : "idle";
      }
    }
    session.lastError = formatError(lastError);
    // Roll back the optimistic bubble so a rejected send does not leave a
    // phantom user message.
    const last = session.items[session.items.length - 1];
    if (last && last.type === "userMessage" && last.text === text) session.items.pop();
    force((v) => v + 1);
    return false;
  };

  const interrupt = () => {
    const id = store.currentId;
    if (!id) return;
    clientRef.current?.request("turn/interrupt", { threadId: id }).catch((e) => {
      store.view(id).session.lastError = formatError(e);
      force((v) => v + 1);
    });
  };

  const changeTheme = (next: Theme) => {
    // 立即生效，再落盘。
    const prev = theme;
    themeTouchedRef.current = true;
    setTheme(next);
    clientRef.current?.request("ui/settings/write", { theme: next }).catch((e) => {
      // 写失败：回退乐观更新，让 UI 与服务端持久化的权威值保持一致，同时照旧
      // 报错（无打开的 thread 时 setCurrentError 是 no-op，回退就是唯一的反馈）。
      setTheme(prev);
      setCurrentError(formatError(e));
      force((v) => v + 1);
    });
  };

  // Sidebar badges: server-authoritative status per thread + unread marker for
  // threads that finished a turn while the user was looking elsewhere.
  const statuses = new Map<string, ThreadStatus>();
  const unread = new Map<string, TurnStatus>();
  for (const g of groups)
    for (const t of g.threads) {
      const v = store.peek(t.thread_id);
      if (!v) continue;
      statuses.set(t.thread_id, v.status);
      if (v.unread && t.thread_id !== currentId)
        unread.set(t.thread_id, v.session.lastStatus ?? "completed");
    }

  const approval = current?.approval ?? null;
  const bannerItems = store
    .pendingApprovalsElsewhere()
    .filter((r) => !dismissedApprovals.current.has(r.id));

  useEffect(() => {
    applyTheme(theme);
  }, [theme]);

  return (
    <>
      <div className="flex h-screen flex-col bg-surface text-fg">
        <TitleBar />
        <div className="flex min-h-0 flex-1 flex-row">
        <ThreadSidebar
          groups={groups}
          workspaces={workspaces}
          currentId={currentId}
          pinned={pinned}
          statuses={statuses}
          unread={unread}
          onSelect={selectThread}
          onRename={renameThread}
          onDelete={deleteThread}
          onTogglePin={onTogglePin}
          onReorderPinned={onReorderPinned}
          onNew={onNew}
          onRemoveWorkspace={removeWorkspace}
          onBrowse={onBrowse}
          boards={boards}
          boardSummaries={boardSummaries}
          selectedBoard={selectedBoard}
          onCreateBoard={(path) => void onCreateBoard(path)}
          onRemoveBoard={(path) => void onRemoveBoard(path)}
          onOpenBoard={onOpenBoard}
          onOpenSettings={() => setSettingsOpen(true)}
        />
        <div className="relative flex min-w-0 flex-1 flex-col">
          {/* 看板是主区域的一个视图，不是一个常驻列：选中才出现，且问的是
              被选中那个项目。没有选中时主区域还是原来的对话。 */}
          {selectedBoard !== null && boardCollapsed && (
            <SuperpowersKanbanCollapsedBar
              board={selectedBoard}
              onExpand={() => setBoardCollapsed(false)}
            />
          )}
          {selectedBoard !== null && !boardCollapsed && (
            <section
              aria-label="Superpowers 看板"
              className="flex max-h-[60%] shrink-0 flex-col overflow-y-auto border-b border-neutral-800 bg-neutral-925"
            >
              <SuperpowersKanbanSettings
                switchOn={boardOn}
                source={boardSource}
                onToggle={(next) => void onToggleBoardSwitch(next)}
                onCollapse={() => setBoardCollapsed(true)}
              />
              {boardError !== null && (
                <div className="flex items-center gap-3 px-4 pb-3 text-xs text-red-400">
                  <span>{BOARD_ERROR_TEXT[boardError]}</span>
                  {boardError === "not_created" && (
                    <button
                      type="button"
                      onClick={() => void onCreateBoard(selectedBoard)}
                      className="rounded border border-neutral-700 px-2 py-0.5 text-neutral-300 hover:text-neutral-100"
                    >
                      创建看板
                    </button>
                  )}
                </div>
              )}
              <SuperpowersKanbanEnqueue
                pickFile={pickFile}
                enqueue={(spec, plan) =>
                  enqueueBoardCard(boardRpc, selectedBoard, spec, plan)
                }
              />
              <SuperpowersKanbanView
                switchOn={boardOn}
                source={boardSource}
                cards={boardCards}
                pluginMissing={boardPluginMissing}
              />
            </section>
          )}
          <ApprovalBanner
            items={bannerItems}
            onJump={(id) => void selectThread(id)}
            onDismiss={() => {
              for (const r of bannerItems) dismissedApprovals.current.add(r.id);
              force((v) => v + 1);
            }}
          />
          <StatusBar
            cwd={current?.info?.cwd ?? null}
            model={current?.info?.model ?? null}
            status={status}
            usage={current?.session.usage ?? null}
          />
          <ChatView
            items={current?.session.items ?? []}
            error={current?.session.lastError ?? null}
            retrying={current?.session.retrying ?? null}
          />
          <MessageInput
            turnActive={current?.session.turnActive ?? false}
            onSend={send}
            onInterrupt={interrupt}
            mode={current?.mode ?? null}
            onModeChange={setThreadMode}
            onSlashCommand={(name, args) => void onSlashCommand(name, args)}
          />
          {currentId && openSubagent && (
            <SubagentTrace
              taskId={openSubagent}
              row={
                railStore.get(currentId).find((r) => r.taskId === openSubagent) ?? null
              }
              children={childrenOf(railStore.get(currentId), openSubagent)}
              rows={traceRows}
              onClose={() => void closeDetail(currentId)}
              onDrill={(taskId) => void openDetail(currentId, taskId)}
              onMessage={async (taskId, message) => {
                await clientRef.current?.request("agent/message", {
                  threadId: currentId,
                  taskId,
                  message,
                });
              }}
              onCancel={async (taskId) =>
                await clientRef.current!.request<AgentCancelPreviewResult>(
                  "agent/cancel/preview",
                  { threadId: currentId, taskId },
                )
              }
              onConfirmCancel={async (taskId, token) => {
                await clientRef.current?.request("agent/cancel", {
                  threadId: currentId,
                  taskId,
                  confirmationToken: token,
                });
              }}
            />
          )}
          {approval && (
            <ApprovalDialog
              key={approval.id}
              request={approval}
              onDecide={async (decision) => {
                try {
                  await clientRef.current?.respond(approval.id, decision);
                } catch (e) {
                  const s = store.peek(approval.params.thread_id)?.session;
                  if (s) s.lastError = formatError(e);
                } finally {
                  store.clearApproval(approval.params.thread_id);
                  force((v) => v + 1);
                }
              }}
            />
          )}
        </div>
        {currentId && !railCollapsed && (
          <SubagentRail
            rows={railStore.get(currentId)}
            selectedTaskId={openSubagent}
            onOpen={(taskId) => void openDetail(currentId, taskId)}
            onCollapse={() => setRailCollapsed(true)}
          />
        )}
        </div>
      </div>
      <SettingsDialog
        open={settingsOpen}
        theme={theme}
        onThemeChange={changeTheme}
        onClose={() => setSettingsOpen(false)}
      />
    </>
  );
}
