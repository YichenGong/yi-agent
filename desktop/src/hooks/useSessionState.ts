import { useEffect, useRef, useState } from "react";
import { isRemoteClient } from "../lib/platform";
import { writeListCache, type ThreadListCache } from "../lib/sessionCache";
import { formatError } from "../lib/errorMessage";
import type {
  AgentChildrenListResult,
  AgentTraceSnapshotResult,
  ThreadSummary,
  Workspace,
  WorkspaceGroup,
} from "../lib/protocol";
import type { RpcClient } from "../lib/rpc";
import type { SubagentRailStore } from "../lib/subagents";
import type { ThreadStore } from "../lib/threadStore";

/**
 * 会话关注点：会话列表（分组 + 置顶）、最近目录、侧栏抽屉、子 agent 详情栈与
 * 右侧详情栏的开合，以及维持这些状态所需的 ref 影子与读取器。
 *
 * 归属是这里唯一的硬约束：分组、置顶、最近目录、详情栈、栏的开合都必须答得出
 * 「这是**哪条**会话 / 哪个视图的」，故它们与 `currentId` 同属一处。`currentId`
 * 本身也由本 hook 持有并对内消费：`railRows` 用它取暂留区、`openDetail` 的调用点
 * 用它点名会话；对外则要供 `useProcessState`、diff 的归属判据 `currentIdRef`、
 * 以及 App 的会话 JSX（`changeThreadModel` 的 id 等都从这里读）使用。
 *
 * `rpc` 是 4a/4b 那条「取客户端」的 getter：未连接时返回 null，动作**静默返回**。
 * 这条语义逐字保留自搬移前（原为 `const c = clientRef.current; if (!c) return;`）：
 * 列表是附属视图，断线读不到不该在对话里留一条红字，更不该漏出一个没人 catch 的
 * rejection；`refreshThreads` 另把 null 用作「未知」的返回值，调用方必须按未知
 * 处理（见它的注释）。
 *
 * `setCurrentError`/`railStore`/`force` 都不是本关注点的状态，而是 `openDetail` 与
 * `refreshSubagents` 落回 App 层的那几笔写入（对话里的红字、子 agent 暂留区、
 * 强制重渲染 tick），故由调用方注入，不在这里另起影子。`initialListCache` 同理：
 * 它是手机端首屏缓存，本 hook 只拿它给 `groups`/`pinned` 播种（`useRef(...).current`
 * 仍在 App，因为它同时喂给那个跨关注点的预填 effect）。
 *
 * 跨关注点的编排留在 App（R6''）：`selectThread`/`newThread`/`deleteThread`/`send`/
 * `removeWorkspace`/`renameThread`/`onTogglePin`/`onReorderPinned`/`setThreadMode` 等
 * 动作，以及生命周期 effect（通知回调、握手建列表）都留在那里，只把引用改成
 * `sess.*`——它们要同时动看板、diff、进程与内容缓存，不属于会话这一处。
 */
export function useSessionState({
  rpc,
  store,
  force,
  setCurrentError,
  railStore,
  initialListCache,
}: {
  /** 取当前的 RPC 客户端；未连接即 null（各动作据此静默返回）。 */
  rpc: () => RpcClient | null;
  /** 会话存储：列表刷新要按全集 seed，写入缓存要读当前会话 id。 */
  store: ThreadStore;
  /** App 的强制重渲染 tick（附属视图就地更新后需要它）。 */
  force: (updater: (v: number) => number) => void;
  /** 往当前会话的 session 上写一条错误（`openDetail` 的失败路径）。 */
  setCurrentError: (msg: string) => void;
  /** App 的子 agent 暂留区（`refreshSubagents` 的回包落盘处），也是 `railRows` 的数据源。 */
  railStore: SubagentRailStore;
  /** 手机端首屏列表缓存；桌面端为 null（初值保持空，行为与从前逐字节一致）。 */
  initialListCache: ThreadListCache | null;
}) {
  const [currentId, setCurrentId] = useState<string | null>(null);
  const [groups, setGroups] = useState<WorkspaceGroup[]>(initialListCache?.groups ?? []);
  // 服务端排好序的全部置顶会话（从顶到底）。置顶项仍留在各自的分组内，侧栏
  // 渲染分组时需自行过滤，否则会重复渲染。
  const [pinned, setPinned] = useState<ThreadSummary[]>(initialListCache?.pinned ?? []);
  const [workspaces, setWorkspaces] = useState<Workspace[]>([]);
  const [sidebarOpen, setSidebarOpen] = useState(false);
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
  // 通知回调在 `wireClient` 的闭包里只注册一次，读不到渲染期最新的 currentId；
  // `ui/gitDiff/focus` 允许 threadId 为 null（"聚焦当前会话"），此时必须知道此刻
  // 选中的是谁，故与 openTaskId 一样放进 ref。
  const currentIdRef = useRef<string | null>(null);
  useEffect(() => {
    currentIdRef.current = currentId;
  }, [currentId]);
  // 同上：`thread/titleChanged` 的处理也在 `wireClient` 的一次性闭包里，读不到最新
  // 的 groups/pinned。标题就地更新需要「当前列表」做底，故各放一个 ref 影子。
  const groupsRef = useRef<WorkspaceGroup[]>(groups);
  const pinnedRef = useRef<ThreadSummary[]>(pinned);
  useEffect(() => {
    groupsRef.current = groups;
  }, [groups]);
  useEffect(() => {
    pinnedRef.current = pinned;
  }, [pinned]);
  // 右侧会话详情栏的开合与当前 Tab。栏由状态栏那颗图标开合；Tab 还受两条外部
  // 路径驱动——点子 agent 卡片（回到「子 agent」Tab 的轨迹）与模型推送的
  // `ui/gitDiff/focus`（切到 diff）。
  const [panelOpen, setPanelOpen] = useState(false);
  const [detailTab, setDetailTab] = useState<"subagents" | "diff" | "processes">("subagents");

  const refreshThreads = async (): Promise<WorkspaceGroup[] | null> => {
    const c = rpc();
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
      if (isRemoteClient()) writeListCache(localStorage, r.groups, r.pinned ?? [], store.currentId);
      return r.groups;
    } catch {
      // 列表刷新失败不打断对话。返回 null 表示"未知":调用方必须把 null
      // 当作未知处理,不得回退成 normal。注意权限模式不会因此自动恢复——
      // 它保持未知,直到下一次切换线程时重新从 listAll 回读。
      return null;
    }
  };

  const refreshWorkspaces = async () => {
    const c = rpc();
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
    const c = rpc();
    if (!c) return;
    // 点卡片即回到右栏：打开栏并把 Tab 拨回「子 agent」（它可能正停在 Git Diff）。
    setPanelOpen(true);
    setDetailTab("subagents");
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
      await rpc()?.request("agent/trace/unwatch", { threadId });
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
    const c = rpc();
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
   * 分组写操作。四个动作的失败处理一致：写进会话错误并强制重渲染，返回
   * `false` 让调用方（侧栏的就地编辑器）知道该保留输入框。
   *
   * 成功路径**不**就地改本地 `groups`：服务端紧接着广播 `thread/groupsChanged`，
   * `App` 收到后重拉列表。就地改一份、再被重拉覆盖，会让输入框在两次数据之间
   * 闪一下。
   */
  const groupCall = async (method: string, params: Record<string, unknown>): Promise<boolean> => {
    const c = rpc();
    if (!c) return false;
    try {
      await c.request(method, params);
      return true;
    } catch (e) {
      setCurrentError(formatError(e));
      force((v) => v + 1);
      return false;
    }
  };

  /**
   * 建组。成功时返回新组的 id——侧栏据此把新框**立即展开**并进入改名态：
   * 框默认折叠，建完不展开的话，用户只能看到一个「▸ 组名 2」，看不出
   * 两条会话确实都进去了（这正是此前「一条在里一条在外」的观感来源）。
   */
  const createGroup = async (
    workspace: string,
    name: string,
    description: string,
    threadIds: string[],
  ): Promise<string | null> => {
    const c = rpc();
    if (!c) return null;
    try {
      const r = await c.request<{ group: { id: string } }>("thread/group/create", {
        workspace,
        name,
        description,
        threadIds,
      });
      return r.group?.id ?? null;
    } catch (e) {
      setCurrentError(formatError(e));
      force((v) => v + 1);
      return null;
    }
  };

  const updateGroup = (
    workspace: string,
    groupId: string,
    patch: { name?: string; description?: string },
  ) => groupCall("thread/group/update", { workspace, groupId, ...patch });

  const setGroupMembers = (workspace: string, groupId: string, threadIds: string[]) =>
    groupCall("thread/group/setMembers", { workspace, groupId, threadIds });

  const deleteGroup = (workspace: string, groupId: string) =>
    groupCall("thread/group/delete", { workspace, groupId });

  return {
    // state：会话 JSX、`selectThread`/`newThread` 等编排者与生命周期 effect 都要用。
    currentId,
    groups,
    pinned,
    workspaces,
    sidebarOpen,
    detailStack,
    traceRows,
    openSubagent,
    // 当前对话的子 agent 行。状态栏入口（数量徽标、是否有内容）与右侧面板共用同一份，
    // 故在此派生一次，不在 JSX 里重复取。
    railRows: currentId ? railStore.get(currentId) : [],
    panelOpen,
    detailTab,
    // setters：编排者与 JSX 直接写它们（切会话收起栏、切 Tab、标题就地更新等）。
    setCurrentId,
    setGroups,
    setPinned,
    setWorkspaces,
    setSidebarOpen,
    setDetailStack,
    setTraceRows,
    setPanelOpen,
    setDetailTab,
    // refs：App 的一次性通知闭包直接读——`agent/trace/event` 认领当前任务，
    // `ui/gitDiff/focus` 认领当前会话，`thread/titleChanged` 认领当前列表。
    openTaskId,
    currentIdRef,
    groupsRef,
    pinnedRef,
    // readers：App 的动作与生命周期 effect 都要调（切会话刷新、进详情、拉目录）。
    refreshThreads,
    refreshWorkspaces,
    openDetail,
    closeDetail,
    refreshSubagents,
    // 分组动作：侧栏的多选建组、就地编辑、组菜单都走这四个。
    createGroup,
    updateGroup,
    setGroupMembers,
    deleteGroup,
  };
}
