import { useEffect, useRef, useState } from "react";
import { isRemoteClient } from "../lib/platform";
import { DESKTOP_THREAD_LIST_CACHE_KEY, writeListCache, type ThreadListCache } from "../lib/sessionCache";
import { formatError } from "../lib/errorMessage";
import type {
  AgentChildrenListResult,
  ThreadSummary,
  Workspace,
  WorkspaceGroup,
} from "../lib/protocol";
import type { RpcClient } from "../lib/rpc";
import type { SubagentRailStore } from "../lib/subagents";
import type { ThreadStore } from "../lib/threadStore";
import type { ViewAnchor } from "../lib/viewAnchor";

/**
 * 会话关注点：会话列表（分组 + 置顶）、最近目录、侧栏抽屉、右侧详情栏的开合，
 * 以及维持这些状态所需的 ref 影子与读取器。
 *
 * 归属是这里唯一的硬约束：分组、置顶、最近目录、栏的开合都必须答得出
 * 「这是**哪条**会话 / 哪个视图的」，故它们与 `currentId` 同属一处。`currentId`
 * 本身也由本 hook 持有并对内消费：`railRows` 用它取暂留区；对外则要供
 * `useProcessState`、diff 的归属判据 `currentIdRef`、以及 App 的会话 JSX
 * （`changeThreadModel` 的 id 等都从这里读）使用。
 *
 * **子 agent 详情不在这里**：它的选中任务、轨迹行、草稿与两阶段取消属于
 * `hooks/useSubagentTrace.ts` 的控制器。放回本 hook 就必然要在这里再存一份
 * 「当前打开的任务」，而两份状态一旦不同步，通知就会落进错误的视图。
 *
 * `rpc` 是 4a/4b 那条「取客户端」的 getter：未连接时返回 null，动作**静默返回**。
 * 这条语义逐字保留自搬移前（原为 `const c = clientRef.current; if (!c) return;`）：
 * 列表是附属视图，断线读不到不该在对话里留一条红字，更不该漏出一个没人 catch 的
 * rejection；`refreshThreads` 另把 null 用作「未知」的返回值，调用方必须按未知
 * 处理（见它的注释）。
 *
 * `setCurrentError`/`railStore`/`force` 都不是本关注点的状态，而是
 * `refreshSubagents` 落回 App 层的那几笔写入（对话里的红字、子 agent 暂留区、
 * 强制重渲染 tick），故由调用方注入，不在这里另起影子。`initialListCache` 同理：
 * 它是首屏缓存，本 hook 只拿它给 `groups`/`pinned` 播种（`useRef(...).current`
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
  cacheLocal,
  pairingVersion,
}: {
  /** 取当前的 RPC 客户端；未连接即 null（各动作据此静默返回）。 */
  rpc: () => RpcClient | null;
  /** 会话存储：列表刷新要按全集 seed，写入缓存要读当前会话 id。 */
  store: ThreadStore;
  /** App 的强制重渲染 tick（附属视图就地更新后需要它）。 */
  force: (updater: (v: number) => number) => void;
  /** 往当前会话的 session 上写一条错误（分组写动作的失败路径）。 */
  setCurrentError: (msg: string) => void;
  /** App 的子 agent 暂留区（`refreshSubagents` 的回包落盘处），也是 `railRows` 的数据源。 */
  railStore: SubagentRailStore;
  /** 首屏列表快照；本地 desktop 与 paired remote 分键存储。 */
  initialListCache: ThreadListCache | null;
  /** Whether the active desktop host is local (remote slots must not persist). */
  cacheLocal?: () => boolean;
  /** 配对版本号；每次成功配对/重配自增。用于丢弃上一个服务端的会话列表。 */
  pairingVersion: number;
}) {
  const [currentId, setCurrentId] = useState<string | null>(null);
  const [groups, setGroups] = useState<WorkspaceGroup[]>(initialListCache?.groups ?? []);
  // 服务端排好序的全部置顶会话（从顶到底）。置顶项仍留在各自的分组内，侧栏
  // 渲染分组时需自行过滤，否则会重复渲染。
  const [pinned, setPinned] = useState<ThreadSummary[]>(initialListCache?.pinned ?? []);
  const [workspaces, setWorkspaces] = useState<Workspace[]>([]);
  const [sidebarOpen, setSidebarOpen] = useState(false);
  // 通知回调在 `wireClient` 的闭包里只注册一次，读不到渲染期最新的 currentId；
  // `ui/gitDiff/focus` 允许 threadId 为 null（"聚焦当前会话"），此时必须知道此刻
  // 选中的是谁，故与 `groupsRef` 一样放进 ref。
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

  /**
   * 每条会话的阅读位置。`null` = 上次离开时贴底；没有键 = 从没记过。
   *
   * 放 **ref** 不放 state：`ChatView` 每轮滚动都会上报，若进 state 就是滚动期
   * 一次重渲染。它只在切会话时被读一次（`ChatView` 的 `anchor` 属性），
   * 天然不需要驱动渲染。
   */
  const viewAnchors = useRef(new Map<string, ViewAnchor | null>());

  // 重配后必须丢掉上一个服务端的列表：这是同一个 App 实例续跑，不是冷启动，
  // `store` 换成新的了，但这些 state 属于本 hook，跟着一起清。
  //
  // 判据是「版本号变了」，不是「首帧已过」：
  //   - groups/pinned 的初值来自 initialListCache（首屏画缓存的来源），首帧
  //     执行本 effect 时**不能**清掉它；
  //   - `main.tsx` 用 `<React.StrictMode>` 包着 `<App/>`，开发模式下 effect 会
  //     跑两遍（挂载 → 清理 → 再挂载）。若用 `useRef(false)` 记「首帧已过」，
  //     第二遍就会把首屏缓存清空——手机上表现为「开发构建首屏永远是空的」，
  //     而生产构建不复现，极难排查。
  //   - 版本号比对对两种情形都安全：挂载时相等 → 直接返回；自增时不等 → 清一次
  //     并把新值记下来，重复跑不再清。
  //
  // 也不能判断 `pairingVersion === 0`：那把语义绑在「0 是特殊值」上，将来若
  // 持久化该值，初值不再是 0，就会静默失效且无任何报错。
  const lastPairingVersion = useRef(pairingVersion);
  useEffect(() => {
    if (lastPairingVersion.current === pairingVersion) return;
    lastPairingVersion.current = pairingVersion;
    // 列表与选中项。`workspaces` 是最近目录，同样属于上一台机器。
    setCurrentId(null);
    setGroups([]);
    setPinned([]);
    setWorkspaces([]);
    setPanelOpen(false);
    // 阅读位置按 thread_id 记，A 的 id 在 B 上没有意义，留着只是占内存。
    viewAnchors.current.clear();
  }, [pairingVersion]);

  // Keep the last actual selection, not just the selection at list refresh time.
  useEffect(() => {
    if (isRemoteClient() || !cacheLocal?.() || (!groups.length && !pinned.length)) return;
    writeListCache(localStorage, groups, pinned, currentId, DESKTOP_THREAD_LIST_CACHE_KEY);
  }, [groups, pinned, currentId, cacheLocal]);

  const refreshThreads = async (): Promise<WorkspaceGroup[] | null> => {
    const c = rpc();
    if (!c) return null;
    try {
      const r = await c.request<{ groups: WorkspaceGroup[]; pinned?: ThreadSummary[] }>(
        "thread/listAll",
        {},
      );
      // A late list belongs to the connection that requested it, not whichever
      // host is active now. Never render or persist another host's response.
      if (rpc() !== c) return null;
      setGroups(r.groups);
      setPinned(r.pinned ?? []);
      // seed 喂全集：置顶项仍在分组里，所以不会漏；store 的视图与侧栏的分区
      // 划分无关（状态徽章、未读点都按 thread_id 查 store）。
      store.seed(r.groups.flatMap((g) => g.threads));
      if (isRemoteClient()) writeListCache(localStorage, r.groups, r.pinned ?? [], store.currentId);
      else if (cacheLocal?.()) writeListCache(localStorage, r.groups, r.pinned ?? [], store.currentId, DESKTOP_THREAD_LIST_CACHE_KEY);
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

  /** 记下某条会话此刻的阅读位置（`null` = 贴底）。 */
  const rememberView = (threadId: string, anchor: ViewAnchor | null) => {
    viewAnchors.current.set(threadId, anchor);
  };

  /**
   * 某条会话的阅读位置。**没记过返回 `undefined`**——它与「记过且贴底」（`null`）
   * 语义不同：前者走首次进入的默认（到底），后者明确表示「我看的是最新」。
   */
  const anchorFor = (threadId: string): ViewAnchor | null | undefined =>
    viewAnchors.current.get(threadId);

  /** 会话被删时丢弃它的位置，免得列表里再出现同名 id 时被旧锚点劫持。 */
  const forgetView = (threadId: string) => {
    viewAnchors.current.delete(threadId);
  };

  return {
    // state：会话 JSX、`selectThread`/`newThread` 等编排者与生命周期 effect 都要用。
    currentId,
    groups,
    pinned,
    workspaces,
    sidebarOpen,
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
    setPanelOpen,
    setDetailTab,
    // refs：App 的一次性通知闭包直接读——`ui/gitDiff/focus` 认领当前会话，
    // `thread/titleChanged` 认领当前列表。子 agent 详情不再经过这里：它的归属
    // 判据在 `useSubagentTrace` 的控制器里，闭包读不到 React state 也照样安全。
    currentIdRef,
    groupsRef,
    pinnedRef,
    // readers：App 的动作与生命周期 effect 都要调（切会话刷新、进详情、拉目录）。
    refreshThreads,
    refreshWorkspaces,
    rememberView,
    anchorFor,
    forgetView,
    refreshSubagents,
    // 分组动作：侧栏的多选建组、就地编辑、组菜单都走这四个。
    createGroup,
    updateGroup,
    setGroupMembers,
    deleteGroup,
  };
}
