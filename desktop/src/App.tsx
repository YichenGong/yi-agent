import { useCallback, useEffect, useRef, useState } from "react";
import { isIos, isRemoteClient } from "./lib/platform";
import { type StorageLike, storedRemoteConfig } from "./lib/remoteConfig";
import { useIsMobile } from "./lib/useIsMobile";
import { useDiffState } from "./hooks/useDiffState";
import { useProcessState } from "./hooks/useProcessState";
import { useBoardState } from "./hooks/useBoardState";
import { useSessionState } from "./hooks/useSessionState";
import { useSettingsState } from "./hooks/useSettingsState";
import { RpcClient, type RequestFn } from "./lib/rpc";
import { defaultRedeem } from "./pairing";
import { ThreadStore } from "./lib/threadStore";
import { transportFactory } from "./transportFactory";
import { ChatView } from "./components/ChatView";
import { CacheNotice } from "./components/CacheNotice";
import { MessageInput } from "./components/MessageInput";
import { ModelPicker } from "./components/ModelPicker";
import { StatusBar } from "./components/StatusBar";
import { ApprovalDialog } from "./components/ApprovalDialog";
import { ApprovalBanner } from "./components/ApprovalBanner";
import { ThreadSidebar } from "./components/ThreadSidebar";
import { ThreadDetailPanel } from "./components/ThreadDetailPanel";
import { TitleBar } from "./components/TitleBar";
import type {
  AgentCancelPreviewResult,
  Item,
  ThreadStatus,
  ThreadSummary,
  TurnStatus,
  WorkspaceGroup,
} from "./lib/protocol";
import { childrenOf, SubagentRailStore } from "./lib/subagents";
import { applyTagChange, applyTitleChange } from "./lib/threadListUpdate";
import { DEFAULT_TAG_NAMES, resolveTagNames, type TagId } from "./lib/threadTags";
import { SubscriptionWindow } from "./lib/subscriptionWindow";
import { formatError } from "./lib/errorMessage";
import {
  ATTACHMENT_EXTENSIONS,
  attachmentProblem,
  fileNameOf,
  imageAttachmentProblem,
  imageSizeProblem,
  IMAGE_EXTENSIONS,
  pendingKindFromPath,
  type PendingAttachment,
} from "./lib/attachmentLimits";
import { nextReconnectDelay } from "./lib/reconnect";
import { uploadImage, isHeic } from "./lib/imageUpload";
import { pickImageFiles } from "./lib/imagePicker";
import { SuperpowersKanbanView } from "./components/SuperpowersKanbanView";
import { SuperpowersKanbanSettings } from "./components/SuperpowersKanbanSettings";
import { SuperpowersKanbanEnqueue } from "./components/SuperpowersKanbanEnqueue";
import {
  enqueueBoardCard,
  enqueuePrequeuedCard,
  promoteBoardCard,
  setBoardSwitch,
  watchmanFromSettings,
  writeBoardWatchman,
} from "./lib/superpowersKanbanSwitch";
import { type BoardErrorKind, boardErrorKind } from "./lib/boardIndex";
import { createBoard, removeBoard } from "./lib/superpowersKanbanBoards";
import { pluginProjectPaths } from "./lib/pluginSettings";
import { threadStartParams } from "./lib/threadStart";
import {
  permissionModeFromResponse,
  setPermissionModeParams,
  type ThreadMode,
} from "./lib/threadPermissionMode";
import { renderHelp } from "./lib/slash";
import { configReadParams } from "./lib/configRequest";
import { estimateCost, formatCost } from "./lib/pricing";
import { parseTheme } from "./lib/theme";
import { SettingsDialog } from "./components/SettingsDialog";
import { PairingScreen, needsPairing } from "./components/PairingScreen";
import { OnboardingWizard } from "./components/OnboardingWizard";
import { onboardingStatus } from "./lib/onboarding";
import { readListCache, writeListCache } from "./lib/sessionCache";
import { createContentStore, persistContent, type AsyncContentStore, type ContentEntry } from "./lib/contentStore";
import { parseSyncPlan, resolveSyncPlan, type SyncAction } from "./lib/syncPlan";
import { useDragDropPaths } from "./hooks/useDragDropPaths";
import { subscribeDragDrop } from "./lib/dragDropSubscribe";
import { notifyModelCapabilitiesChanged } from "./lib/modelCapabilitiesBus";
import type { ReasoningCapabilities } from "./lib/models";
import { insertDroppedPaths } from "./lib/droppedPaths";

/**
 * 看板失败的三种说法。
 *
 * 两种可操作的状态各自点名下一步：进程不可达就说明是连接问题而不是插件问题，
 * 插件没装才让用户去装。`other` 不猜原因。**没有「未创建」这一支**：宿主不再
 * 发这个码——「有没有看板」由登记表（`resident/list`）表达，不是一个错误。
 */
const BOARD_ERROR_TEXT: Record<BoardErrorKind, string> = {
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

/**
 * 引导写盘后重启侧车，让新 `.env` 生效（侧车启动时把 `cfg`/清单读进内存）。
 * 远端/网页客户端没有 Tauri 也没有侧车，直接跳过；重启失败也不阻塞——配置
 * 已在磁盘上，宿主重启后自然生效。
 */
async function restartSidecar() {
  if (isRemoteClient()) return;
  try {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("restart_sidecar");
  } catch {
    // 见上：失败不阻塞。
  }
}

/** 读不到全局 storage 时的空实现（隐私模式等），与 `lib/platform.ts` 同形。 */
const emptyStorageLike: StorageLike = {
  getItem: () => null,
  setItem: () => {},
  removeItem: () => {},
};

/** 凭据被服务端明确拒绝（直连 4401）时的文案；与「配对码无效」区分开。 */
export const REPAIR_REMOVED_TEXT = "此设备已被桌面移除，请重新配对";

export default function App() {
  // Per-thread state lives in a mutable store; the `force` tick is how React
  // learns that a view changed (the views are mutated in place, not setState'd).
  const storeRef = useRef(new ThreadStore());
  const store = storeRef.current;
  const [, force] = useState(0);
  const clientRef = useRef<RpcClient | null>(null);
  // Set once the effect wires up; `RpcClient` calls it to re-handshake the current
  // client after the relay bridge restarts (see `RpcClient.request`). A ref, not
  // a closure over `clientRef`, so the client created inside the effect captures
  // the latest one.
  const recoveryRef = useRef<((raw: RequestFn) => Promise<void>) | null>(null);
  const inited = useRef(false);
  /**
   * 取当前的 RPC 客户端；未连接即 null。
   *
   * 供附属视图的读取（diff）当接缝：拿不到客户端就静默返回——附属视图断线读不到，
   * 不该在对话里留一条红字，也不该漏出一个没人 catch 的 rejection（故不学
   * `pluginCall` 那几个接缝的 `Promise.reject`）。只闭合 ref，空依赖即稳定。
   */
  const rpc = useCallback(() => clientRef.current, []);
  // 首屏是否为「缺组长的项目目录」补建过组长。组长是项目级固定入口，一个进程
  // 只补一次即可：服务端创建幂等，且补过之后该目录的分组里就有组长条目了。
  // 用 ref 而非 state：它不参与渲染，且需要跨多次握手/重连保持「已补过」。
  const leadEnsured = useRef(false);
  // Threads whose history has already been replayed (resumed) this session;
  // switching back to them must not replay again.
  const warm = useRef(new Set<string>());
  // Resume requests currently on the wire, so a rapid second click cannot
  // interleave two histories into one session.
  const inFlightResume = useRef(new Set<string>());
  // Remote-only (S2): the LRU of threads we keep warm via `thread/subscribe`.
  // Content notifications are only delivered for subscribed threads, so the
  // window bounds how many histories stream live; the rest stay cold and are
  // caught up on demand with `thread/readItems`.
  const subWindow = useRef(new SubscriptionWindow());
  // 本连接里已为「plan 判 none 的缓存会话」补过尾部只读 readItems 的会话。按**连接**
  // 记账（`connect()` 建新客户端时清空）：这样任何一次重连——sidecar 重启、或 socket
  // 掉了但进程还在（经中继的瞬断、切回前台）——都会为这些会话重补一次；而同一连接内
  // 切回它们不再重复发请求。若改用 `subWindow`（订阅窗口）判定则漏掉后一种重连
  // （窗口只在 `exited` 时清），正是「warm idle 会话永远停在缓存尾部」的残留缺口。
  const toppedUp = useRef(new Set<string>());
  // Approvals the user has dismissed from the banner; keyed by approval id.
  const dismissedApprovals = useRef(new Set<string>());
  // 每个会话待发送的附件。按会话隔离，与草稿同理：切走时留在原会话、切回时
  // 原样出现，绝不会把 A 的附件发到 B 上。
  const [pending, setPending] = useState<Record<string, PendingAttachment[]>>({});
  const [status, setStatus] = useState<string>("connecting");
  // 手机端首屏缓存：权威列表在服务端，这里只放一份同步快照，让侧栏在
  // `thread/listAll` 回来之前就能画出来。桌面端不读（`null`），初值保持 []，
  // 行为与从前逐字节一致。
  const initialListCache = useRef(isRemoteClient() ? readListCache() : null).current;
  /** 标签显示名（6 键齐全）。默认名来自 threadTags.ts，服务端只给覆盖项。 */
  const [tagNames, setTagNames] = useState<Record<TagId, string>>(DEFAULT_TAG_NAMES);
  /**
   * 宿主的构建类型（`build/info`）。`null` = 还不知道，此时**不**渲染「实验功能」
   * Tab——宁可晚一帧出现，也不要在 stable 上闪一个必然报错的面板。
   * 这个值在一次连接内不会变，故握手时读一次就够。
   */
  const [buildKind, setBuildKind] = useState<"dev" | "release" | "nogit" | null>(null);
  // 子 agent 暂留区:按对话隔离的折叠状态 + 用户是否收起了它。收起是纯 UI 选择,
  // 不进 store;数据本身仍随通知累积,展开即是当前值。
  const railStore = useRef(new SubagentRailStore()).current;
  // 手机端：会话侧栏与子 agent 栏是抽屉，不是常驻列，否则聊天区被挤到看不清。
  const isMobile = useIsMobile();
  // 手机端离线 = 尚未握手成功。桌面端恒为 false（不显示缓存相关 UI）。
  const offline = isRemoteClient() && status !== "connected";
  // 内容缓存（IndexedDB）：仅手机端。懒建，桌面端永不构造（`null`）。
  const contentStoreRef = useRef<AsyncContentStore | null>(null);
  const getContentStore = (): AsyncContentStore => (contentStoreRef.current ??= createContentStore());
  // 防抖落盘定时器，按 thread 隔离：一个会话的写入不能被另一个的盖掉。
  const persistTimers = useRef(new Map<string, number>());
  // 内容来自缓存、尚未被服务端覆盖的会话 → 其 `savedAt`（供离线提示）。
  const cachedAt = useRef(new Map<string, number>());
  /** 每个会话的缓存元数据（epoch / 游标 / 是否被裁剪），用于组装 hints 与裁决。 */
  const cacheMetaRef = useRef(new Map<string, ContentEntry>());
  /** 本连接的 syncPlan（`initialize` 的响应），按 threadId 索引。 */
  const syncPlanRef = useRef(new Map<string, { action: SyncAction; epoch: number }>());

  /**
   * 从内容缓存预填某会话的对话（手机端）。服务端 item 到达后按 id 就地替换，
   * 所以这里的旧内容只会短暂停留；若服务端已先填好了内容则整段跳过。
   *
   * 预填**不**置 warm：那会让连上后的 `thread/resume` 被跳过，服务端历史
   * 永远合不进来。缓存（cachedAt）与 warm 是两套东西，必须分开。
   */
  const prefillFromCache = (id: string) => {
    if (!isRemoteClient()) return;
    const store0 = getContentStore();
    void store0
      .read(id)
      .then((entry) => {
        if (!entry || entry.items.length === 0) return;
        // 记下元数据供 hints 与裁决使用（即使本次不注入内容）。
        cacheMetaRef.current.set(entry.threadId, entry);
        const v = store.peek(id);
        if (!v) return;
        // 已被判过过期（世代变化）⇒ 绝不把旧内容注回（spec §6.4）。
        if (v.session.epochInvalidated) return;
        if (v.session.items.length > 0) return; // 服务端已经填过
        v.session.upsertItems(entry.items);
        v.session.lastServerItemId = v.session.lastServerItemId ?? entry.lastServerItemId;
        cachedAt.current.set(id, entry.savedAt);
        force((n) => n + 1);
      })
      .catch(() => {
        // 预填是附加能力：读失败保留空态，由 resume 兜底。
      });
  };

  /**
   * Declare what this client holds, on the very first frame (spec §4.1).
   * Only remote clients send it; the desktop path stays byte-identical.
   */
  const buildResumeHints = () => {
    const metas = [...cacheMetaRef.current.values()];
    return metas
      .sort((a, b) => b.savedAt - a.savedAt)
      .slice(0, 20)
      .map((e) => ({
        threadId: e.threadId,
        contentEpoch: e.contentEpoch ?? 0,
        lastItemId: e.lastServerItemId,
        cachedAt: e.savedAt,
      }));
  };

  // 冷启首帧：把缓存里已有的条目读进内存元数据，供首次 initialize 的 hints 使用。
  // `contentStore.keys()` 已存在；读失败静默跳过（hints 是优化，不能挡住握手）。
  useEffect(() => {
    if (!isRemoteClient()) return;
    const cs = getContentStore();
    void (async () => {
      try {
        for (const id of await cs.keys()) {
          const entry = await cs.read(id);
          if (entry) cacheMetaRef.current.set(entry.threadId, entry);
        }
      } catch {
        /* hints 是优化：读失败就按"没有缓存"声明 */
      }
    })();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  /** 节流：内容变更后延迟落盘，避免逐字 delta 每次都写 IndexedDB。 */
  const CONTENT_PERSIST_DEBOUNCE_MS = 800;
  const schedulePersist = (id: string) => {
    if (!isRemoteClient()) return;
    const timers = persistTimers.current;
    const existing = timers.get(id);
    if (existing !== undefined) window.clearTimeout(existing);
    const timer = window.setTimeout(() => {
      timers.delete(id);
      void flushContent(id);
    }, CONTENT_PERSIST_DEBOUNCE_MS);
    timers.set(id, timer);
  };
  /** 立即落盘某会话（切走 / 页面隐藏 / 卸载时用）。 */
  const flushContent = (id: string) => {
    if (!isRemoteClient()) return Promise.resolve();
    const v = store.peek(id);
    if (!v) return Promise.resolve();
    return persistContent(getContentStore(), id, v.session).catch(() => {
      // 缓存写失败不影响任何用户可见行为。
    });
  };
  /** 删除某会话的内容缓存（仅手机端）。桌面端不构造内容缓存，故此处也必须门控。 */
  const deleteCachedContent = (id: string) => {
    if (!isRemoteClient()) return;
    void getContentStore().delete(id).catch(() => {});
  };

  /** Write an error onto the current thread's session (if any). */
  const setCurrentError = (msg: string) => {
    const s = store.current()?.session;
    if (s) s.lastError = msg;
  };

  // 会话关注点（当前会话 id/会话列表与置顶/最近目录/侧栏抽屉/详情栈与右侧
  // 详情栏开合，连同它们的 ref 影子与读取器）整体搬进 hook，见
  // `hooks/useSessionState.ts`。跨关注点的编排不在这里：`selectThread`/
  // `newThread`/`deleteThread`/`send` 那些动作与生命周期 effect 留在 App，
  // 只把引用改成 `sess.*`。顺序：`sess` 必须排在 `proc`/`diff` 之前——
  // `proc` 取它的 `currentId`、`diff` 取它的 `currentIdRef`，二者都是「此刻
  // 选中谁」的判据，而这份状态正是本 hook 持有的。
  const sess = useSessionState({ rpc, store, force, setCurrentError, railStore, initialListCache });
  // 会话级后台进程关注点（列表/详情 id/增量输出 + 详情轮询与列表重拉）整体搬进
  // hook，见 `hooks/useProcessState.ts`。详情 Tab 的开合不在这里：它属会话关注点，
  // 由下面切 Tab 与 `selectThread` 几处调用点自己收起详情。
  const proc = useProcessState({ rpc, store, force, currentId: sess.currentId });
  // 会话级 diff 关注点（快照/加载态/错误/下钻态 + 归属判据）整体搬进 hook，
  // 见 `hooks/useDiffState.ts`。面板开合不在这里：它属会话关注点，由下面几处
  // 调用点在重置 diff 的同一现场自己收起。
  const diff = useDiffState({ rpc, currentIdRef: sess.currentIdRef });
  // 看板关注点（看板页状态/侧栏登记表与摘要/后台值守）整体搬进 hook，见
  // `hooks/useBoardState.ts`。跨关注点的编排不在这里：`selectThread`/`newThread`
  // 的「离开看板」、`onCreateBoard` 等几个动作（要写会话错误位与 force）、以及
  // 握手后拉登记表与注册 2 秒计时器那两个现场，都留在 App，只把引用改成 `board.*`。
  const board = useBoardState({ clientRef });

  // 设置关注点（设置页/配对界面开合、首启引导、本机中继地址、主题、重配提示）
  // 整体搬进 hook，见 `hooks/useSettingsState.ts`。跨关注点的编排不在这里：
  // 生命周期 effect 里那几处（跟随 `ui/settings/updated`、采纳首屏 read、4401
  // 进重配、握手成功重新武装提示）与 JSX（`SettingsDialog`/`PairingScreen`/
  // `OnboardingWizard`，后者的 `onDone` 还要 `restartSidecar`/`modelCall`）都留在
  // App，只把引用改成 `prefs.*`。`changeTheme` 的写失败要落一条对话红字并强制
  // 重渲染，故 `force`/`setCurrentError` 作为参数注入。
  const prefs = useSettingsState({ rpc, force, setCurrentError });

  /**
   * 插件 Tab 的宿主接缝。必须稳定：它只闭合 `clientRef`（一个 ref），所以
   * 空依赖即可。若每次渲染都换新函数身份，插件面板的加载 effect 会跟着重跑，
   * 看板的 2 秒轮询就把用户没保存的输入冲掉。
   */
  const pluginCall = useCallback(
    (method: string, params: unknown): Promise<unknown> => {
      const c = clientRef.current;
      if (!c) return Promise.reject(new Error("not connected"));
      return c.request(method, params);
    },
    [],
  );

  /**
   * 模型 Tab 的宿主接缝。理由同 `pluginCall`：只闭合 `clientRef`，空依赖即稳定；
   * 若每次渲染换新身份，「模型」面板的读取 effect 会跟着重跑，把用户没保存的
   * 编辑冲掉。
   */
  const modelCall = useCallback(
    (method: string, params: unknown): Promise<unknown> => {
      const c = clientRef.current;
      if (!c) return Promise.reject(new Error("not connected"));
      return c.request(method, params);
    },
    [],
  );

  /**
   * 「技能」Tab 的宿主接缝。理由同 `pluginCall`：只闭合 `clientRef`，空依赖即稳定；
   * 若每次渲染换新身份，技能面板的读取 effect 会跟着重跑，把已展开的正文与滚动
   * 位置一起冲掉。
   */
  const skillsCall = useCallback(
    (method: string, params: unknown): Promise<unknown> => {
      const c = clientRef.current;
      if (!c) return Promise.reject(new Error("not connected"));
      return c.request(method, params);
    },
    [],
  );

  /**
   * 「MCP」Tab 的宿主接缝。理由同 `pluginCall`：只闭合 `clientRef`，空依赖即稳定；
   * 若每次渲染换新身份，MCP 面板的读取 effect 会跟着重跑，把已展开的工具清单与
   * 折叠状态一起冲掉。
   */
  const mcpCall = useCallback(
    (method: string, params: unknown): Promise<unknown> => {
      const c = clientRef.current;
      if (!c) return Promise.reject(new Error("not connected"));
      return c.request(method, params);
    },
    [],
  );

  /**
   * 「实验功能」Tab 的宿主接缝。理由同 `pluginCall`：只闭合 `clientRef`，空依赖
   * 即稳定；换新身份会让面板的读取 effect 重跑。
   */
  const flagsCall = useCallback(
    (method: string, params?: unknown): Promise<unknown> => {
      const c = clientRef.current;
      if (!c) return Promise.reject(new Error("not connected"));
      return c.request(method, params ?? {});
    },
    [],
  );

  /**
   * 图片读取的宿主接缝（`image/read`）。理由同 `pluginCall`：只闭合 `clientRef`，
   * 空依赖即稳定。`ChatView` 把它透传给每个气泡/工具卡的 `useImageData`，而那个
   * effect 依赖 `call`——一旦每次渲染换新函数身份，每张图都会重新分片拉取。
   */
  const imageCall = useCallback(
    (method: string, params: unknown): Promise<unknown> => {
      const c = clientRef.current;
      if (!c) return Promise.reject(new Error("not connected"));
      return c.request(method, params);
    },
    [],
  );

  const current = sess.currentId ? store.view(sess.currentId) : null;

  // 首屏：把缓存里的会话播种进 store（标题/cwd/model/状态徽标），并预填选中会话
  // 的内容。预填**不**置 warm：那会让连上后的 `thread/resume` 被跳过，服务端历史
  // 永远合不进来。
  const prefilledOnce = useRef(false);
  useEffect(() => {
    const c = initialListCache;
    if (!c || prefilledOnce.current) return;
    prefilledOnce.current = true;
    store.seed([...c.pinned, ...c.groups.flatMap((g) => g.threads)]);
    if (c.selectedId && c.groups.some((g) => g.threads.some((t) => t.thread_id === c.selectedId))) {
      store.select(c.selectedId);
      sess.setCurrentId(c.selectedId);
      void prefillFromCache(c.selectedId);
    }
    force((v) => v + 1);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // 连上即清"内容来自缓存"的标记——已改为在该会话对账完成后按会话清除
  // （selectThread 的 resume/readItems 收尾），否则另一个仍在显示缓存的会话
  // 会失去标注（spec §5.3）。

  // 页面隐藏 / 卸载时把在途的会话内容立刻写盘：手机切后台后可能很久不再执行
  // 定时器，等着防抖会把最后一段内容丢掉。
  useEffect(() => {
    if (!isRemoteClient()) return;
    const flushAll = () => {
      for (const id of persistTimers.current.keys()) {
        window.clearTimeout(persistTimers.current.get(id));
      }
      const ids = new Set([...persistTimers.current.keys(), ...store.ids()]);
      persistTimers.current.clear();
      for (const id of ids) void flushContent(id);
    };
    const onHide = () => {
      if (document.visibilityState === "hidden") flushAll();
    };
    document.addEventListener("visibilitychange", onHide);
    window.addEventListener("pagehide", flushAll);
    return () => {
      document.removeEventListener("visibilitychange", onHide);
      window.removeEventListener("pagehide", flushAll);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  /**
   * Switch the visible thread. A warm thread (already resumed) only swaps the
   * view — its timeline kept accumulating in the background. A cold thread is
   * resumed once; per-thread isolation means the base session list/sidebar can
   * keep serving other threads in parallel.
   */
  const selectThread = async (
    id: string,
    opts?: { leaveBoard?: boolean; forceResume?: boolean },
  ) => {
    const prevId = store.currentId;
    store.select(id);
    sess.setCurrentId(id);
    // 冷会话先拿缓存内容垫上（有则显示、无则空），随后 resume/readItems 覆盖。
    // 判据是 warm 与在途 resume，与「是否有缓存」无关：预填**不**置 warm，所以
    // 这里不会因为命中缓存而跳过 resume。这里刻意放在下方的早退分支（running
    // 会话走 readItems 而非 resume）之前——那条路径同样要先垫上缓存。
    if (!warm.current.has(id) && !inFlightResume.current.has(id)) prefillFromCache(id);
    // 切走即把上一条会话的在途内容落盘（切回时预填的就是它）。
    if (prevId && prevId !== id) void flushContent(prevId);
    // 会话页成为当前页：离开看板（若正在看）。这样点侧栏任意会话即「跳转」
    // 过去，不必再点一次「收起看板」。setSelectedBoard 幂等，不在看板时无害。
    // 仅当这是「用户切到会话」时才是离场：重连后重放选中的那条会话不代表用户
    // 换了页，此时必须留在看板上（opts.leaveBoard === false），否则一次 sidecar
    // 重启就会把人从看板踢回会话。
    if (opts?.leaveBoard !== false) board.setSelectedBoard(null);
    // 手机端选中会话即收起抽屉，把宽度让回聊天区。
    if (isMobile) sess.setSidebarOpen(false);
    sess.setPanelOpen(false);
    diff.resetThreadDiff();
    force((v) => v + 1);
    // 该对话的子 agent 列表:重进对话时重新拉取,免得依赖"通知一定到过"。
    void sess.refreshSubagents(id);
    // 该对话的后台进程:重进对话时重新拉取。
    void proc.refreshProcesses(id);
    // 切对话即关闭详情:详情属于特定进程,留着会指向别的对话的运行结果。
    proc.setOpenProcessId(null);
    proc.setProcessOutput(null);
    // 服务端对本次会话的裁决（spec §4.2）。注意 plan 以 threadId 为键直接查：
    // 被删除的会话不会出现在 listAll 里，靠 summary 反查必然查不到它的 `drop`。
    // `noneNeedsCatchUp`：裁决为 none 的缓存会话仍**可能落后于服务端尾部**——
    // none 只表示「缓存不必作废」，不是「已是最新」（spec §4.2）。锁住这一点后
    // 由下方的只读 readItems 补齐（不打断在途回合），否则会永远停在缓存尾部。
    let noneNeedsCatchUp = false;
    if (isRemoteClient()) {
      const planEntry = syncPlanRef.current.get(id);
      const cached = cacheMetaRef.current.get(id) ?? null;
      const view = store.peek(id);
      if (view) {
        const summary =
          sess.pinned.find((t) => t.thread_id === id) ??
          sess.groups.flatMap((g) => g.threads).find((t) => t.thread_id === id) ??
          null;
        // 先取服务端裁决（plan 覆盖该会话时以它为准，否则按 listAll 版本兜底）。
        const resolved =
          planEntry?.action ?? resolveSyncPlan({ plan: syncPlanRef.current, summary, cached });
        // 客户端例外必须在**调用点**再判一次：`resolveSyncPlan` 内部那条同名例外
        // 会被 `planEntry?.action` 短路掉（plan 覆盖该会话时根本不进那个函数），
        // 于是「缓存被裁剪过 ⇒ 即使服务端正确地说 none 也要全量补齐头部」就失效，
        // 用户会静默看到一个被截断的会话（spec §4.2 例外）。这条只依赖客户端自己
        // 知道的信息（自己有没有裁过），故与 plan 是否覆盖无关。
        const action: SyncAction =
          resolved === "none" && cached?.trimmed ? "full" : resolved;
        // 把缓存条目的已知世代先记为本地已知世代：否则本会话首见时
        // `session.contentEpoch` 仍是 null，`full`（世代已变）会被 adoptEpoch
        // 当作"首见"只记录、不作废，幽灵内容留在屏上（spec §4.2）。
        // 老条目缺 `contentEpoch`（未知）时不播种 ⇒ 保持 null ⇒ 只记录不作废，
        // 「缺字段=保守」且不误删内容。
        if (cached?.contentEpoch != null && view.session.contentEpoch === null) {
          view.session.contentEpoch = cached.contentEpoch;
        }
        if (action === "drop") {
          // 会话已不存在：丢掉本地状态与缓存，回落第一条。
          store.drop(id);
          cacheMetaRef.current.delete(id);
          void getContentStore().delete(id);
          cachedAt.current.delete(id);
          if (sess.currentIdRef.current === id) {
            const first = sess.pinned[0] ?? sess.groups.flatMap((g) => g.threads)[0];
            if (first) void selectThread(first.thread_id, { leaveBoard: false });
          }
          return;
        }
        const epoch = planEntry?.epoch ?? summary?.content_epoch ?? 0;
        if (action === "none" && cached) {
          // 缓存有效：不必整段重放，但**不表示已是最新**——用户手上的内容很可能
          // 落后于服务端尾部（两端并存的常态，spec §4.2/§7.2）。置 `warm` 让下方
          // 的 resume 守卫接管，同时**置位补齐标记**：下方会为它做一次只读
          // `thread/readItems`（不打断任何在途回合），把落后的尾部补回来。
          view.session.adoptEpoch(epoch, cached.lastServerItemId);
          warm.current.add(id);
          noneNeedsCatchUp = true;
        } else {
          // full：按世代作废。epoch 取服务端的权威值（0 仅表示"未知"，用于触发作废）。
          view.session.adoptEpoch(epoch, null);
        }
      }
    }
    // **看**一个运行中的会话绝不走 `thread/resume`：resume 会先中断在跑的
    // turn（服务端 `interrupt_and_wait_for_persist`），而「点开看一眼」是纯
    // 被动动作，不该有把别人打断的副作用。改用只读的 `thread/readItems` 补齐
    // 内容，之后靠既有的通知流继续跟新。这对桌面端同样成立——桌面端过去没有
    // 这层保护（它被 `isRemoteClient()` 关在远端分支里），于是点开一张正在跑
    // 的看板卡就会把它打断。
    const curView = store.peek(id);
    const running =
      curView?.status === "running" || curView?.status === "awaiting_approval";

    // Remote-only (S2): keep this thread in the warm subscription window.
    // Desktop (stdio) never subscribes: its single client receives everything.
    //
    // `forceResume`（stale_context 重同步）：锚点失效意味着服务端内容已整段变化，
    // 必须走**全量** resume 重新对账。此前的 warm / 订阅窗口 / running 三道早退都会
    // 让 resume 永不发生（当前会话必在订阅窗口内），故置位时全部跳过（spec §7.3）。
    if (isRemoteClient()) {
      const inWindow = subWindow.current.has(id);
      const win = subWindow.current.touch(id);
      const c0 = clientRef.current;
      if (c0) {
        void c0.request("thread/subscribe", { threadIds: win }).catch(() => {});
        // 冷+在跑（原逻辑）：内容确定要被替换，收紧门槛。
        const catchingUp = !inWindow && running && !opts?.forceResume;
        // plan 判 none 的缓存会话：服务端已判缓存有效（spec §4.2），落后是正常
        // 并存形状（§7.2）——故补齐是**静默**的，不挂「正在同步」也不拦发送，
        // 但仍要补一次尾部，否则用户永远停在缓存尾部却以为内容是最新的。
        // 每连接每会话只补一次：`toppedUp` 在 `connect()` 建新客户端时清空，故任何
        // 一次重连都会重补；`forceResume` 例外（那是 stale_context 触发的全量重放，
        // 另行会发 `thread/resume`，再补一次只读 `readItems` 是多余的）。
        const silentTopUp =
          noneNeedsCatchUp && !catchingUp && !toppedUp.current.has(id) && !opts?.forceResume;
        if (silentTopUp) toppedUp.current.add(id);
        if (catchingUp || silentTopUp) {
          const after = store.peek(id)?.session.lastServerItemId ?? null;
          // 对账开始：内容随时可能是旧的，UI 与发送门槛据此收紧（spec §5.1）。
          if (curView && !silentTopUp) curView.syncState = "syncing";
          void (async () => {
            try {
              const r = await c0.request<{ items: Item[] }>(
                "thread/readItems",
                after ? { threadId: id, afterItemId: after } : { threadId: id },
              );
              const v = store.peek(id);
              if (v) {
                v.session.upsertItems(r.items);
                force((n) => n + 1);
              }
            } catch {
              // 补齐是附加能力:读失败就保留已有内容,不改动会话状态。
            } finally {
              // 无论成败都必须收敛，否则用户会永久卡在"正在同步"（spec §11 风险 3）。
              const v = store.peek(id);
              if (v) {
                if (!silentTopUp) {
                  v.syncState = "synced";
                  // 该会话已与服务端对过账：去掉"这是缓存"的标注（spec §5.3）。
                  // 静默补齐保留标注：内容仍源自缓存，离线时理应继续标注（§5.3）。
                  cachedAt.current.delete(id);
                }
                force((n) => n + 1);
              }
            }
          })();
        }
      }
      // Already warm, or caught up via readItems above → view-only swap.
      // `forceResume` 例外：全量重同步必须落到下面的 resume 分支（spec §7.3）。
      if ((inWindow || running) && !opts?.forceResume) return;
    }
    if (running && !isRemoteClient()) {
      // 桌面端同样成立：**看**一个运行中的会话绝不走 `thread/resume`（resume 会先
      // 中断在跑的 turn），改用只读的 `thread/readItems` 补齐内容。这不设 `syncing`
      // 也不挂提示条：桌面端没有缓存，内容恒来自服务端。
      // （远端 running 已在上面的远端分支内处理并早退；唯一走到这里的远端情形是
      // `forceResume`——那是 stale_context 触发的全量重放，必须落到 resume。）
      const c0 = clientRef.current;
      if (c0) {
        const after = store.peek(id)?.session.lastServerItemId ?? null;
        void (async () => {
          try {
            const r = await c0.request<{ items: Item[] }>(
              "thread/readItems",
              after ? { threadId: id, afterItemId: after } : { threadId: id },
            );
            const v = store.peek(id);
            if (v) {
              v.session.upsertItems(r.items);
              force((n) => n + 1);
            }
          } catch {
            // 补齐是附加能力:读失败就保留已有内容,不改动会话状态。
          }
        })();
      }
      return;
    }
    if ((warm.current.has(id) || inFlightResume.current.has(id)) && !opts?.forceResume) {
      return; // warm → 只切视图
    }
    const c = clientRef.current;
    if (!c) return;
    // 对账开始：全量回放在途，内容随时可能是旧的，UI 与发送门槛据此收紧（spec §5.1）。
    const coldView = store.peek(id);
    if (coldView) coldView.syncState = "syncing";
    inFlightResume.current.add(id);
    // 全量回放期间收集服务端 item id；成功后据此删掉缓存里服务端已经没有的项
    // （clear / compact / 裁剪造成的幽灵内容）。readItems 只补尾部，不走这条路径。
    const replaySession = store.view(id).session;
    replaySession.beginReplay();
    try {
      const resumed = await c.request<{
        permission_mode?: ThreadMode;
        reasoning_effort?: string | null;
      }>("thread/resume", {
        threadId: id,
      });
      const replayed = replaySession.endReplay();
      // 只有真收到过回放帧才允许删项。空集是不能下「服务端没有这些 item」结论的：
      // 它既可能是「服务端历史本来就空」，也可能是「这趟 resume 压根没投递帧」。
      // 而后者的代价是把刚预填的缓存整段抹掉——spec §5 的三段式第一步（先显示
      // 缓存）就此消失。生产 ws 路径上全量回放必然携带帧(thread/started +
      // items/completed),所以这一守卫只在「无帧可对账」时跳过,不影响幽灵项清理。
      if (replayed !== null && replayed.size > 0) replaySession.dropStale(replayed);
      // 删除竞态:resume 在途时用户可能已删掉该冷 thread,此时不能再把它
      // 加回 warm(会复活已删 id)。peek 不创建视图,仅判断是否仍存在。
      const view = store.peek(id);
      if (!view) return;
      warm.current.add(id);
      // 对账完成：内容已是服务端权威，解禁发送并去掉"这是缓存"的标注（spec §5.1/§5.3）。
      view.syncState = "synced";
      cachedAt.current.delete(id);
      void flushContent(id);
      // 响应直接带权威 mode：一次请求即定,不受 listAll 快照的竞态/失败影响
      // （「手机上打开会话时 YOLO 概率性没同步过来」的修复点）。
      const fromResponse = permissionModeFromResponse(resumed);
      if (fromResponse !== null) view.mode = fromResponse;
      // effort 同样以 resume 回包为权威：服务端报的是**实际建出**的 agent 的生效
      // **值**（清过覆盖时 meta 是 null，但工厂已回落条目默认，故回包仍是实际值）。
      // 这个值是「生效值」而非「覆盖值」——无覆盖时它就等于条目默认，本字段便
      // 无法自己区分两者。控件（ModelPicker）拿清单里的条目默认与之比对折回覆盖：
      // 相等即当「无覆盖」，显示「未设置（继承 X）」，不会把默认档错误打勾。故这里
      // 如实记下服务端报告的值即可（旧服务端不带该字段时记 null）。
      view.reasoningEffort = typeof resumed.reasoning_effort === "string" ? resumed.reasoning_effort : null;
      // listAll 回读仅作兜底（旧服务端不带该字段时）；缺该 thread 时保留已知值,
      // 不把已确定的模式打回 null。
      const gs = await sess.refreshThreads();
      if (gs !== null) view.mode = modeForThread(gs, sess.pinned, id) ?? view.mode;
      force((v) => v + 1);
    } catch (e) {
      replaySession.endReplay();
      // 同样地,失败路径只在视图仍存在时写错误,避免 re-create 一个已被
      // drop 的 ThreadView(无界泄漏 + 残留 warm 条目)。
      const view = store.peek(id);
      if (view) {
        view.session.lastError = formatError(e);
        // 失败也必须收敛，否则用户会永久卡在"正在同步"（spec §11 风险 3）。
        view.syncState = "synced";
      }
      force((v) => v + 1);
    } finally {
      inFlightResume.current.delete(id);
    }
  };

  const newThread = async (cwd?: string) => {
    const c = clientRef.current;
    if (!c) return;
    try {
      const t = await c.request<{
        thread_id: string;
        cwd: string;
        model: string;
        permission_mode?: ThreadMode;
        reasoning_effort?: string | null;
      }>("thread/start", threadStartParams(cwd));
      warm.current.add(t.thread_id);
      store.view(t.thread_id).info = { cwd: t.cwd, model: t.model, model_ref: null };
      // 新会话无覆盖 ⇒ 回包报的是实际**生效值**（解析出的条目默认，或条目/配置都没有
      // 时的 null）。与 resume 同款：值本身分不出「覆盖」与「条目默认」，交给控件按
      // 清单条目默认比对折回覆盖。
      store.view(t.thread_id).reasoningEffort =
        typeof t.reasoning_effort === "string" ? t.reasoning_effort : null;
      // 新会话恒 normal，但以响应字段为准（服务端是权威），缺失时回退 normal。
      store.view(t.thread_id).mode = permissionModeFromResponse(t) ?? "normal";
      store.select(t.thread_id);
      sess.setCurrentId(t.thread_id);
      // 新会话同样是「当前会话变了」：旧 diff 必须退场，否则会挂在新会话上。
      sess.setPanelOpen(false);
      diff.resetThreadDiff();
      // 新建会话同样把焦点带回会话页：与点会话一致。
      board.setSelectedBoard(null);
      force((v) => v + 1);
      // Remote-only (S2): register the fresh thread in the subscription window
      // *now*. It picks a new id (not previously warm) and gets `warm` below, so
      // selectThread would skip it — yet without a subscription its content
      // notifications would be filtered out and the turn would appear to crawl
      // in from `thread/listAll` reads alone.
      if (isRemoteClient()) {
        const win = subWindow.current.touch(t.thread_id);
        void clientRef.current?.request("thread/subscribe", { threadIds: win }).catch(() => {});
      }
      const gs = await sess.refreshThreads();
      if (gs !== null) {
        const v = store.view(t.thread_id);
        v.mode = modeForThread(gs, sess.pinned, t.thread_id) ?? v.mode;
        force((n) => n + 1);
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
    board.setBoardError(null);
    try {
      await createBoard(board.boardRpc, path);
    } catch (e) {
      setCurrentError(formatError(e));
      force((v) => v + 1);
    }
    await board.refreshBoards();
  };

  /**
   * 移除看板。决策 5 说这一下不可逆（队列状态会被删掉），所以先问一句；
   * 用户说不，就连 RPC 都不发。
   */
  const onRemoveBoard = async (path: string) => {
    if (!window.confirm(`移除看板会删除 ${path} 的队列状态，且不可恢复。继续？`)) return;
    board.setBoardError(null);
    try {
      await removeBoard(board.boardRpc, path);
    } catch (e) {
      setCurrentError(formatError(e));
      force((v) => v + 1);
    }
    // 正在看的看板被移除了：主区域不能再展示一个已经不存在的看板。
    if (board.selectedBoard === path) board.setSelectedBoard(null);
    await board.refreshBoards();
  };

  /**
   * 打开看板只改 selectedBoard，不动 currentId。
   *
   * 主区域只有「会话页 / 看板页」两态（见 spec 2026-10-04）：「收起」已被删除，
   * 离开看板统一走「切到某条会话」（selectThread/newThread 清 selectedBoard）。
   */
  const onOpenBoard = (path: string) => {
    // 已经在该看板页：纯 no-op，不重读、不清屏。
    if (path === board.selectedBoard) return;
    // 换项目等于换问题：上一个项目的失败说法和卡片留在屏幕上只会误导。
    board.setBoardError(null);
    board.setBoardCards([]);
    board.setDoneExpanded(false); // 换项目 → 新看板从折叠开始
    board.setSelectedBoard(path);
  };

  /**
   * 写开关。失败不再吞掉：把失败翻译成三种可操作的说法摆出来。
   *
   * 成功才更新本地状态（服务端是权威），失败保持原状并报出原因——原来那句
   * `.catch(() => {})` 正是「点了没反应」的来源。
   */
  const onToggleBoardSwitch = async (next: boolean) => {
    if (board.selectedBoard === null) return;
    board.setBoardError(null);
    try {
      await setBoardSwitch(board.boardRpc, board.selectedBoard, next);
      board.setBoardOn(next);
      board.setBoardSource("project");
    } catch (error) {
      board.setBoardError(boardErrorKind(error));
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
      await sess.refreshWorkspaces();
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
      await sess.refreshWorkspaces();
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
    await sess.refreshThreads();
    await sess.refreshWorkspaces();
  };

  /**
   * 关联文件夹：原生选目录 → `workspace/link` → 刷新分组与最近目录。
   *
   * 关联只在**新建会话时**快照进 `ThreadMeta.extra_roots`，所以这里不需要碰任何
   * 已在跑的会话：它们的根集在设计上就固定了（改关联只影响之后新建的会话）。
   */
  const linkFolder = async (primary: string) => {
    let child: string | null = null;
    try {
      child = await pickDirectory();
      if (!child) return;
      await clientRef.current?.request("workspace/link", { primary, child });
    } catch (e) {
      setCurrentError(formatError(e));
      force((v) => v + 1);
      return;
    }
    // 成功后刷新：附属根要从顶层分组挪进主根的嵌套小节，两份数据都得重新拉。
    await sess.refreshThreads();
    await sess.refreshWorkspaces();
  };

  /** 解除关联。失败也刷新，保证 UI 与服务端一致（与 removeWorkspace 同口径）。 */
  const unlinkFolder = async (child: string) => {
    try {
      await clientRef.current?.request("workspace/unlink", { child });
    } catch (e) {
      setCurrentError(formatError(e));
      force((v) => v + 1);
    }
    await sess.refreshThreads();
    await sess.refreshWorkspaces();
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
    await sess.refreshThreads();
  };

  const deleteThread = async (id: string) => {
    const c = clientRef.current;
    if (!c) return;
    // 删除成功后若 worktree 回收失败，会话本身已经没了、无法在应用内再删一次；
    // 若不在这里弹给用户，残留在磁盘上的那个目录就成了他永远看不见的孤儿。
    // 于是把「删除失败」的成功响应记下来，等确认流程走完再统一告知。
    let failed: { path: string; reason: string } | null = null;
    try {
      // 删除是不可逆的，而这个会话可能还有子代理在跑、或挂着一张卡片 worktree。
      // 服务端在未 force 时只回答「还有几个子代理 / worktree 将删还是留」，不动
      // 任何东西；据此问一句，用户同意后才带 force 重发。子代理活在项目 daemon
      // 里，worktree 是不可逆的磁盘目录，都不能不问自删。
      const first = await c.request<{
        status?: string;
        active_children?: number;
        worktree?: { path: string; action: "remove" | "keep"; reason: string; removed?: boolean };
      }>("thread/delete", { threadId: id });
      if (first?.worktree?.removed === false) {
        failed = { path: first.worktree.path, reason: first.worktree.reason };
      }
      if (first?.status === "needs_confirmation") {
        const count = first.active_children ?? 0;
        const lines: string[] = [];
        if (count > 0) {
          lines.push(`这个会话还有 ${count} 个子代理正在运行，删除会一并终止它们。`);
        }
        if (first.worktree) {
          lines.push(
            first.worktree.action === "remove"
              ? `将一并删除卡片 worktree：${first.worktree.path}（${first.worktree.reason}）`
              : `卡片 worktree 将保留：${first.worktree.reason}`,
          );
        }
        // 兜底：后端返回了需要确认、却没给出任何可读原因时，绝不弹空框。
        if (lines.length === 0) lines.push("删除不可逆。");
        lines.push("继续？");
        const ok = window.confirm(lines.join("\n"));
        if (!ok) return;
        const forced = await c.request<{
          worktree?: { path: string; action: "remove" | "keep"; reason: string; removed?: boolean };
        }>("thread/delete", { threadId: id, force: true });
        if (forced?.worktree?.removed === false) {
          failed = { path: forced.worktree.path, reason: forced.worktree.reason };
        }
      }
    } catch (e) {
      setCurrentError(formatError(e));
      force((v) => v + 1);
      return;
    }
    if (failed) {
      // 会话已删除，应用内无法重删；这里把残留的 worktree 路径与 git 报错原样
      // 告诉用户，让他能自己去处理，而不是让目录无声地留在磁盘上。
      window.alert(`卡片 worktree 未能删除，仍保留在：${failed.path}\n原因：${failed.reason}`);
    }
    // dismissedApprovals 以 approval id 为键;删除 thread 前先摘掉它名下那条
    // 审批,否则按 thread id delete 是 no-op 且会留下永不过期的条目。
    const v = store.peek(id);
    if (v?.approval) dismissedApprovals.current.delete(v.approval.id);
    store.drop(id);
    warm.current.delete(id);
    // 会话已删:内容缓存与在途的落盘/预填登记一并清掉,否则下次冷启会把一个
    // 服务端已经没有的会话画回侧栏里。
    deleteCachedContent(id);
    persistTimers.current.delete(id);
    cachedAt.current.delete(id);
    if (id === sess.currentId) {
      sess.setCurrentId(null);
      // 没有当前会话了，面板与它的 diff 也必须退场：留着就是一片无主的旧内容。
      sess.setPanelOpen(false);
      diff.resetThreadDiff();
    }
    force((v) => v + 1);
    await sess.refreshThreads();
  };

  /** 置顶 / 取消置顶；成功后重拉列表，以服务端顺序为权威。 */
  const onTogglePin = async (id: string, next: boolean) => {
    const c = clientRef.current;
    if (!c) return;
    try {
      await c.request("thread/setPinned", { threadId: id, pinned: next });
      await sess.refreshThreads();
      force((v) => v + 1);
    } catch (e) {
      setCurrentError(formatError(e));
      // 失败也要重拉：本地不做乐观更新，UI 不能停在"看起来置顶了"的状态。
      await sess.refreshThreads();
      force((v) => v + 1);
    }
  };

  /** 设置 / 清除某会话的颜色标签；成功后重拉列表，以服务端为权威。 */
  const onSetTag = async (id: string, tag: TagId | null) => {
    const c = clientRef.current;
    if (!c) return;
    try {
      await c.request("thread/setTag", { threadId: id, tag });
      await sess.refreshThreads();
      force((v) => v + 1);
    } catch (e) {
      setCurrentError(formatError(e));
      // 失败也要重拉：本地不做乐观更新，UI 不能停在「看起来打上了」的状态。
      await sess.refreshThreads();
      force((v) => v + 1);
    }
  };

  /**
   * 保存单个颜色标签的显示名。
   *
   * 只提交改动的那一个键（服务端按键合并），成功后用**服务端**读回的覆盖项重建
   * 本地映射——服务端是权威，本端不自行猜测合并结果。广播随后到达，两端一致。
   */
  const saveTagName = async (tagId: TagId, name: string) => {
    const c = clientRef.current;
    if (!c) return;
    try {
      await c.request("ui/settings/write", { tag_names: { [tagId]: name } });
      force((v) => v + 1);
    } catch (e) {
      setCurrentError(formatError(e));
    }
  };

  /** 置顶分区内拖拽排序落盘；乐观更新 + 失败回滚重拉。 */
  const onReorderPinned = async (orderedIds: string[]) => {
    const c = clientRef.current;
    if (!c) return;
    const prev = sess.pinned;
    const byId = new Map(prev.map((t) => [t.thread_id, t]));
    // 乐观：先按落点顺序重排，`pin_seq` 的权威值仍由服务端在重拉时给出。
    sess.setPinned(orderedIds.map((id) => byId.get(id)).filter((t): t is ThreadSummary => !!t));
    force((v) => v + 1);
    try {
      await c.request("thread/reorderPinned", { threadIds: orderedIds });
    } catch (e) {
      setCurrentError(formatError(e));
      sess.setPinned(prev);
      await sess.refreshThreads();
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
    // iOS 首启还没配对：这一步不能建客户端。`transportFactory` 此时只可能落到
    // `tauriTransport()`，而 iOS 上根本没有 Tauri IPC——那正是要避开的死路。
    // 配对把配置落盘并 bump 版本号后本 effect 重跑，这一次才真的去连 ws。
    if (needsPairing(isIos())) return;
    if (inited.current) return; // guard against React StrictMode double-invoke
    inited.current = true;
    let disposed = false;
    let restartTimer: number | null = null;
    // 连续失败次数：第一次断开排 500ms,此后按 1s/2s/4s/8s 翻倍(封顶 10s)。
    // 握手成功即清零,所以偶尔一次闪断不会让下一次重连背负更大的延迟。
    let attempt = 0;

    // 断开后的重连入口：清掉可能已在途的定时器,立刻建客户端重试。前台唤醒
    // 用它做即时重连,exited 处理则用 scheduleReconnect 排队。
    const reconnectNow = () => {
      if (disposed) return;
      if (restartTimer !== null) {
        window.clearTimeout(restartTimer);
        restartTimer = null;
      }
      let next: RpcClient;
      try {
        next = connect();
      } catch (e) {
        // 建 transport 本身也可能同步抛(落盘 URL 格式坏掉时 `withToken` 里的
        // `new URL` 就会):这同样是"连不上",必须继续退避,否则循环就此死掉。
        setStatus(`error: ${formatError(e)}`);
        scheduleReconnect();
        return;
      }
      void handshake(next);
    };

    // 退避排队：只要组件还挂着就绝不停下重试,后端没起来也不会一直卡在 connecting。
    const scheduleReconnect = () => {
      if (disposed || restartTimer !== null) return;
      restartTimer = window.setTimeout(() => {
        restartTimer = null;
        reconnectNow();
      }, nextReconnectDelay(attempt));
      attempt += 1;
    };

    // iOS 从后台切回前台时 socket 常已被系统静默掐断,状态不会自己回来。
    // 前台唤醒一律重建客户端重连:延迟在这条路径上毫无价值。这里故意不看
    // `connected`——被后台掐断的 socket 可能没有 close/exited 事件,于是那个
    // 标志会陈旧地停在 true(半开连接),信它就会把恰恰要修的场景漏过去。
    // 只在可见性事件上触发,正常会话不受影响;reconnectNow 先清在途定时器,
    // 再替换当前客户端,因此不会并发两条连接。
    const onVisibilityChange = () => {
      if (document.visibilityState !== "visible" || disposed) return;
      attempt = 0;
      reconnectNow();
    };

    // 注册每个客户端的通知/审批/状态回调。sidecar 重启后会换一个新客户端,
    // 这些回调必须随新客户端重新登记(旧客户端的 transport 已随管道断开)。
    const wireClient = (client: RpcClient) => {
      client.onNotification((n) => {
        // A driven session is created outside the desktop's thread/start RPC, so
        // it may not exist in the cached groups when its first Running status
        // arrives. Pull the authoritative list once for an unknown thread; waiting
        // for turn/completed leaves running board cards invisible for the whole run.
        if (
          n.method === "thread/status/updated" &&
          n.params.status === "running" &&
          !sess.groupsRef.current.some((g) =>
            g.threads.some((t) => t.thread_id === n.params.thread_id),
          ) &&
          !sess.pinnedRef.current.some((t) => t.thread_id === n.params.thread_id)
        ) {
          void sess.refreshThreads();
        }
        // 子 agent 的通知不进 ThreadStore:它们是对话的附属视图,不是对话本身。
        if (n.method === "agent/children/updated") {
          railStore.applyNotification(n.params.threadId, n.params.children);
          force((v) => v + 1);
          return;
        }
        if (n.method === "ui/settings/updated") {
          // 对话（set_theme 工具）改了主题：跟随它。同时标记已触碰，免得仍在途
          // 的首屏 read 用旧值把它覆盖回去。服务端推来的即权威值，记作回退基线。
          prefs.themeTouchedRef.current = true;
          const next = parseTheme(n.params.theme);
          prefs.confirmedThemeRef.current = next;
          prefs.setTheme(next);
          return;
        }
        if (n.method === "model/capabilitiesChanged") {
          // 宿主探测完成的权威结果。桌面端没有全局模型清单 store（设置页与输入区
          // 的模型下拉各自持有一份），所以这里只把「哪条变了」广播出去，由各自
          // 重读 `model/list`。协议字段在生成物里是 `unknown`（runtime 类型没有
          // TS 绑定），这里按线上形状断言一次。
          notifyModelCapabilitiesChanged({
            name: n.params.name,
            capabilities:
              (n.params
                .reasoning_capabilities as ReasoningCapabilities | null | undefined) ?? null,
          });
          return;
        }
        if (n.method === "ui/gitDiff/focus") {
          // 模型通过 `show_git_diff` 工具要求聚焦某会话的 diff：`threadId` 可为
          // null（=当前会话），闭包里读不到最新 state，故回退到 ref。
          const target = n.params.threadId ?? sess.currentIdRef.current;
          // 只认当前会话。推送给的是**别人**的 diff 时，直接不产生效果——面板
          // 不打开、Tab 不改、提示语/基准不落，一次 `thread/diff/read` 都不发。
          // 空看总好过错看：没有归属校验时，这条推送会把 `note` 挂到用户正看着
          // 的另一条会话的 diff 上（用户看不到那处改动，却被一段提示指认，是
          // 静默的错配）。`loadThreadDiff` 的归属门挡得住写入，但推送**本身**
          // 不该先劫持当前视图，所以门设在这里。
          if (target === null || target !== sess.currentIdRef.current) return;
          sess.setDetailTab("diff");
          sess.setPanelOpen(true);
          // 提示语随快照走（见 `pendingNoteRef`）：此行只登记挂起值。
          diff.setPendingDiffNote(n.params.note ?? null);
          // 基准不再随通知携带（通知里已没有 `base`）：它现在由**帧**承载。
          // 两条都取，各司其职：
          //  - `loadViewFrame` 拿**模型给的命令帧**（`thread/view/read`）——focus 的
          //    主角，头部命令条与三种原语都来自它；
          //  - `loadThreadDiff` 仍拿结构化快照——它给的是 `commits/files` 列表与
          //    汇总行的 `+X −Y`，也是**无帧**时的正文兜底（旧会话、纯结构化推送）。
          // 帧到了就由帧渲染正文（`GitDiffView`），没到就退回这份快照。
          void diff.loadViewFrame(target);
          void diff.loadThreadDiff(target);
          return;
        }
        if (n.method === "agent/trace/event") {
          // 只接受当前打开任务的流:换任务时旧流可能还有在途帧,丢弃它们比
          // 把两个任务的轨迹拼在一起安全。
          sess.setTraceRows((prev) =>
            n.params.taskId === sess.openTaskId.current ? [...prev, n.params.row] : prev,
          );
          return;
        }
        if (n.method === "process/updated") {
          // 通知只是"该重拉了":它不携带权威数据,重拉 process/list 才是。
          void proc.refreshProcesses(n.params.thread_id);
          return;
        }
        if (n.method === "thread/titleChanged") {
          // 首轮 LLM 标题生成成功后服务端推来的权威标题。通知自带权威值，就地把
          // 它写进本地列表的两份（分组 + 置顶）即可——不必再拉一次全量
          // `thread/listAll`，也与 `thread/modelChanged` 同族。
          const next = applyTitleChange(
            sess.groupsRef.current,
            sess.pinnedRef.current,
            n.params.thread_id,
            n.params.title,
          );
          sess.setGroups(next.groups);
          sess.setPinned(next.pinned);
          force((v) => v + 1);
          return;
        }
        if (n.method === "thread/tagChanged") {
          // 服务端推来的权威标签（null = 已清除）。就地落笔即可，不必重拉列表。
          const next = applyTagChange(
            sess.groupsRef.current,
            sess.pinnedRef.current,
            n.params.thread_id,
            n.params.tag,
          );
          sess.setGroups(next.groups);
          sess.setPinned(next.pinned);
          force((v) => v + 1);
          return;
        }
        if (n.method === "thread/groupsChanged") {
          // 分组变更：通知只带 workspace（组表可能很大），重拉列表即可。
          // 与 tag/title 的「就地落笔」不同——这里没有比整份组表更小的权威值。
          void sess.refreshThreads();
          return;
        }
        if (n.method === "ui/tagNames/updated") {
          // 标签显示名变更（全局帧）：合成完整映射后重渲染，色点 title / 菜单
          // aria-label / 搜索匹配全部随之更新。
          setTagNames(resolveTagNames(n.params.tag_names));
          force((v) => v + 1);
          return;
        }
        // 按 thread_id 路由:后台 thread 的流式输出照常累积,切回去即最新。
        //
        // 只在这一帧**真的有可见变化**时重渲染。bash 每 4 KiB 输出就推一帧
        // `item/delta`，而基线 UI 不渲染工具流式输出；照旧每帧 `force()` 会让长
        // 命令（构建日志、`cat` 大文件）持续把整棵树重绘，主线程被占住时连 Stop
        // 都点不下去。`turn/completed` 例外：它虽已把内容变化算进返回值，但还要
        // 触发下面的列表刷新，所以单独保证重渲染。
        const changed = store.applyNotification(n);
        if (changed || n.method === "turn/completed") force((v) => v + 1);
        // `Notification` 是判别联合：只有带 `thread_id` 的通知才有会话内容可落盘
        // （`error` 无归属、由 store 归到当前会话，在此跳过）。其余 `agent/*`、
        // `ui/*` 通知已在上方分支提前返回。
        if ("thread_id" in n.params) schedulePersist(n.params.thread_id);
        if (n.method === "turn/completed") void sess.refreshThreads();
      });
      client.onApproval((r) => {
        store.setApproval(r);
        force((v) => v + 1);
      });
      client.onStatus((s) => {
        setStatus(s.state);
        // 直连时服务端用 4401 明确拒绝这个 token（ws.rs 的 close_unauthorized）：
        // 重试一万次也不会有别的结局，直接把用户送上重配界面。中继路径不会出现
        // 4401（中继不校验手机 token），故该分支只在直连生效。
        //
        // 守卫只用 ref 判断"是否已在重配界面"：`setStatus` 刚把状态置为 exited，
        // 用 status 判断此刻恒真；闭包里的 status 也是旧快照。普通断线（1006 等）
        // 不在此分支，继续走既有退避重连。
        if (
          isRemoteClient() &&
          s.code === 4401 &&
          !prefs.repairingRef.current &&
          !prefs.repairDismissedRef.current
        ) {
          prefs.setRepairNotice(REPAIR_REMOVED_TEXT);
          prefs.setRepairing(true);
        }
        // sidecar 退出后宿主会自动重启它(bridge 的监管循环)。重连不能依赖用户
        // 重启 App:等新进程起来、状态回到 connecting 时重新握手。断开时清掉
        // warm 缓存,免得切回旧对话时跳过 resume(新进程并不记得任何会话)。
        if (s.state === "exited" && !disposed) {
          // 这个客户端已经死了：立刻注销它的 transport 监听。Tauri 的
          // `app-server://message` 是全局事件名，宿主拉起新侧车后，陈旧监听照样
          // 会收到新进程的帧——不摘掉就会与重连后的新客户端各应用一遍（逐字流
          // 重复，直到 item/completed 按 id 整条替换才"收敛"）。
          client.dispose();
          // 这次握手已经废了:重新从 base 起算退避(handshake 成功时本就会归零,
          // 这里针对的是"连上后立刻断"——attempt 不能被永久抬到高位)。
          warm.current.clear();
          // 推送来的运行状态同样不再成立：新进程对任何会话都没有内存 agent。
          // 不清它的话，一条崩溃前正在跑、而 `listAll` 现已报 `idle` 的会话会
          // 停在 `running`，`selectThread` 会因此跳过 resume，新进程里那个 agent
          // 永远不被重建，之后发送撞上 `-32011` 且无法自愈。详见
          // `ThreadStore.forgetPushStatuses`。
          store.forgetPushStatuses();
          // 同样的理由:新进程不记得任何订阅,窗口必须清空,否则重连后
          // 返回"看起来还 warm"的会话时不会再发 subscribe,内容通知被静默过滤。
          subWindow.current = new SubscriptionWindow();
          // 新连接要为「none 缓存会话」重补一次尾部（见 `toppedUp` 的说明）。
          toppedUp.current.clear();
          scheduleReconnect();
        }
      });
    };

    // 一次完整握手:任何一次重连都要重放它,因为新进程的记忆是空的。
    const handshake = async (client: RpcClient) => {
      try {
        const initialized = await client.request<Record<string, unknown>>("initialize", {
          ...(isRemoteClient() ? { resumeHints: buildResumeHints() } : {}),
        });
        syncPlanRef.current = parseSyncPlan(initialized);
        // 构建类型只在 dev 上开放「实验功能」；读失败按 release 处理（fail-closed，
        // 与 Rust 侧的硬闸同一口径），不让面板因为一次网络抖动冒出来。
        try {
          const info = await client.request<{ build_kind?: unknown }>("build/info", {});
          const kind = info.build_kind;
          setBuildKind(kind === "dev" || kind === "nogit" ? kind : "release");
        } catch {
          setBuildKind("release");
        }
        const settings = await client.request<{
          theme?: unknown;
          resident_daemon_watchman_enabled?: unknown;
          /** 通用化之前的旧字段名：旧宿主仍只回它。 */
          board_watchman_enabled?: unknown;
          relay_url?: unknown;
          tag_names?: unknown;
        }>("ui/settings/read", {});
        // 中继地址没有「用户正在改」的触碰标记：设置页是模态的，首屏 read 回来
        // 时用户还没法编辑它，直接采纳权威值即可。未配置（null）→ 空串。
        prefs.setRelayUrl(typeof settings.relay_url === "string" ? settings.relay_url : "");
        // 只有在此之后没有更新的主题选择时才采纳权威值：read 在途期间用户改了
        // 主题或收到 ui/settings/updated，更新的那个才是当前选择。
        if (!prefs.themeTouchedRef.current) {
          const authoritative = parseTheme(settings.theme);
          prefs.confirmedThemeRef.current = authoritative;
          prefs.setTheme(authoritative);
        }
        // 值守：直接读这份 settings（同一趟请求里就有），不再多发一次
        // `ui/settings/read`。读序（新名优先、旧名兜底）与缺省口径都在
        // `watchmanFromSettings` 里，与 readBoardWatchman 共用同一份实现。
        // 同样只在用户还没拨过开关时采纳：重启重放握手时面板已在屏上，read 在途
        // 期间拨下的开关不能被这趟迟到的旧值悄悄覆盖回原样。
        if (!board.watchmanTouchedRef.current) {
          board.setWatchmanEnabled(watchmanFromSettings(settings as Record<string, unknown>));
        }
        // 标签显示名：服务端只回用户覆盖项，与默认名合并成完整 6 键。
        setTagNames(
          resolveTagNames(
            settings.tag_names && typeof settings.tag_names === "object"
              ? (settings.tag_names as Record<string, string>)
              : null,
          ),
        );
        // 首启引导：读不到状态(宿主老、RPC 报错、字段缺失)一律按「不需要」处理，
        // 绝不因为一个读失败弹向导、更不挡住主界面。
        try {
          const status = await onboardingStatus((m, p) =>
            (clientRef.current as RpcClient).request(m, p),
          );
          prefs.setOnboarding({ needed: status.needed && !status.dismissed, reasons: status.reasons });
        } catch {
          prefs.setOnboarding({ needed: false, reasons: [] });
        }
        await sess.refreshWorkspaces();
        const list = await client.request<{
          groups: WorkspaceGroup[];
          pinned?: ThreadSummary[];
        }>("thread/listAll", {});
        sess.setGroups(list.groups);
        sess.setPinned(list.pinned ?? []);
        store.seed(list.groups.flatMap((g) => g.threads));
        if (isRemoteClient()) writeListCache(localStorage, list.groups, list.pinned ?? [], store.currentId);
        // 已有会话的项目目录不会被服务端的 `listAll` 懒创建覆盖（那会改写它既
        // 有的分组语义），故这类项目的组长入口要靠客户端补：对「一个组长都没
        // 有」且确实存在的目录组逐个 `thread/lead`，再重取一次列表把新组长带出
        // 来。**不 await 在首屏关键路径上**——补组长是锦上添花，绝不能推迟首屏
        // 渲染或扣住后续的 selectThread；失败一律忽略。只在整个进程里补一次
        // （`leadEnsured`），服务端创建本身幂等。
        if (!leadEnsured.current) {
          const needsLead = list.groups.filter(
            (g) => g.exists && !g.threads.some((t) => t.role === "lead"),
          );
          if (needsLead.length > 0) {
            leadEnsured.current = true;
            void (async () => {
              for (const g of needsLead) {
                try {
                  await client.request("thread/lead", { cwd: g.workspace });
                } catch {
                  // 单个目录补建失败只影响它自己的组长条目，继续下一个。
                }
              }
              try {
                const patched = await client.request<{
                  groups: WorkspaceGroup[];
                  pinned?: ThreadSummary[];
                }>("thread/listAll", {});
                sess.setGroups(patched.groups);
                store.seed(patched.groups.flatMap((g) => g.threads));
              } catch {
                // 重取失败就保留首屏那一份：至少侧栏照常可用。
              }
            })();
          }
        }
        // 置顶分区在最上方，服务端给的顺序就是首屏该选中的第一个。
        // 手机端：握手（含重连/切回前台）不是「用户换了页面」，当前会话——冷启从
        // 缓存恢复的、或用户正看着的——必须保持，把它踢回第一条会让人丢掉正在看
        // 的东西。并且重连后新进程记忆为空（warm 与订阅窗口在 exited 时已清），
        // 必须对它再走一次 selectThread：冷则补 resume、running 则补 readItems
        // （不打断在途回合）、warm 则补 subscribe，否则视图留着而该会话的实时更新
        // 会静默停摆。仅当当前会话不在服务端新列表里（被删）或根本没有已选会话时，
        // 才回落第一条。桌面端保留原行为（每次选第一条）。
        // （副作用：selectThread 会 resetThreadDiff()，故重连时 diff 面板若开着会
        // 收起——会话未变，属既有行为，且窄屏下 diff 面板本就不渲染。）
        const keptId = isRemoteClient() ? sess.currentIdRef.current : null;
        const kept =
          keptId !== null &&
          list.groups.some((g) => g.threads.some((t) => t.thread_id === keptId));
        if (kept) {
          await selectThread(keptId!, { leaveBoard: false });
        } else {
          const first = (list.pinned ?? [])[0] ?? list.groups.flatMap((g) => g.threads)[0];
          if (first) await selectThread(first.thread_id, { leaveBoard: false });
        }
        // 否则保持空态,等用户选目录新建(设计 §7.2:不再自动在 $HOME 建对话)。
        // 握手走通才算真的连上:清零退避,下一次断开从 500ms 重新起算。
        attempt = 0;
        setStatus("connected");
        // 真的连上了：重新武装"凭据被移除"的自动提示。此前用户可能已点过返回
        // （那次拒绝的对象是旧凭据）；此后凭据若再失效，是一件新事，该再提示一次。
        prefs.repairDismissedRef.current = false;
        // 看板登记表要等握手完成后再拉：`resident/list` 是普通请求，服务端在
        // `initialize` 之前一律以 not_initialized 拒绝。早拉一次会被拒、把
        // boards 清空，而登记表只在这里拉一次，于是整场会话侧栏都没有看板条目。
        await board.refreshBoards();
      } catch (e) {
        const msg = formatError(e);
        setCurrentError(msg);
        setStatus(`error: ${msg}`);
        // 握手失败同样是"没连上"：必须排队重试，否则会永久停在 error。
        // （经中继时"电脑未连接"正是这条路径——见 rpc.ts 的无 id 错误帧。）
        scheduleReconnect();
      }
    };

    // 经中继时的自愈钩子：服务端把本连接判为「未初始化」时，`RpcClient` 会调用
    // 它重新建立会话，再自动重发那条被拒的请求。
    //
    // 为什么需要它：全 session 只有**一条**电脑侧桥接连接被所有手机共享，
    // app-server 换进程后桥接重连会换成新 `ws-<uuid>`（未握手），而手机的
    // socket 挂在中继上从未断开、不会自己重发 `initialize`。切 session 因此
    // 撞上 `-32010`（详见 `RpcClient.request`）。
    //
    // 新进程的记忆是空的，所以要重放的不只是 `initialize`：服务端只在订阅过的
    // 会话上推送内容，不重放 `thread/subscribe` 会让后台会话静默停更。`raw` 是
    // 不触发自动恢复的裸请求，重放过程中再被拒就直接失败，不会自等待。
    recoveryRef.current = async (raw) => {
      await raw("initialize", {});
      const win = subWindow.current.current();
      if (win.length > 0) await raw("thread/subscribe", { threadIds: win });
    };

    const connect = (): RpcClient => {
      // Tear down the previous client before building the new one. A Tauri (or
      // ws) transport keeps delivering frames until it is explicitly disposed,
      // so leaving the old client subscribed would apply every notification
      // twice: an append-only `item/delta` duplicates the streamed text, and it
      // only "converges" because `item/completed` replaces the whole item by id.
      clientRef.current?.dispose();
      // 新连接 = 新的对账周期：为「none 缓存会话」的静默补齐重新开账（见 `toppedUp`）。
      toppedUp.current.clear();
      const client = new RpcClient(transportFactory(), (raw) => recoveryRef.current!(raw));
      clientRef.current = client;
      wireClient(client);
      return client;
    };

    void handshake(connect());
    // 手机/iPad 切回前台时 socket 多半已被系统掐断,而状态不会自己回来。
    document.addEventListener("visibilitychange", onVisibilityChange);
    // 看板内容由 boardTick 每 2 秒刷新选中项目，读失败绝不影响主流程。
    const boardTimer = window.setInterval(() => board.boardTick.current(), 2000);
    return () => {
      disposed = true;
      if (restartTimer !== null) window.clearTimeout(restartTimer);
      document.removeEventListener("visibilitychange", onVisibilityChange);
      window.clearInterval(boardTimer);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [prefs.pairingVersion]);

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
          // 传当前会话 id：进程级快照的 workdir/model/provider 与会话无关。
          const cfg = await c.request<Record<string, unknown>>(
            "config/read",
            configReadParams(id),
          );
          session.noticeConfig(cfg);
        } catch (e) {
          session.noticeConfig(null, formatError(e));
        }
        break;
      case "clear":
        try {
          await c.request("thread/clear", { threadId: id });
          session.reset();
          // 清空是服务端权威的「历史没了」:本地内容缓存也必须跟着删,否则下次
          // 冷启预填会把刚清掉的内容画回来。
          deleteCachedContent(id);
          cachedAt.current.delete(id);
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
    // 离线时不发：缓存内容可以看，但不能对着它发消息（服务端根本收不到）。
    // 只提示、保留草稿，交给用户连上后再发——不排队、不自动重发。
    // 同样地，会话尚在对账中（syncing）也不能发：屏上可能还是旧缓存，此刻发出去
    // 的上下文与服务端实际上下文可能不一致（spec §5.1）。
    if (offline || store.peek(id)?.syncState === "syncing") {
      store.view(id).session.lastError = offline ? "正在连接，请稍候" : "正在同步，请稍候";
      force((v) => v + 1);
      return false;
    }
    // 这个会话待发送的附件。发送前先取快照：成功要清空它，失败要**留着**重试。
    const files = pending[id] ?? [];
    // 按类别分组：文档走清单 block，图片走内容 block，协议形态不同。
    const images = files.filter((f) => f.kind === "image");
    const documents = files.filter((f) => f.kind !== "image");
    // 附件本身即消息：纯附件(文本框为空)也允许发送。
    if (!text.trim() && files.length === 0) return false;

    const session = store.view(id).session;
    // 乐观回显只带**文档**：图片由服务端在回显的 item 上带 `images` 引用
    // （元数据来自摄取后的落盘），本地此刻并不知道 media_type/size。
    session.addUserMessage(
      text,
      documents.map((f) => ({ name: f.name, path: f.path, size: f.size })),
    );
    force((v) => v + 1);
    // The cached status can be stale: `turn/completed` reaches us before the
    // server flips the thread back to idle, and a listing read inside that
    // window keeps the old value. Send the method the cache implies, then let
    // the server's error code say where it disagreed and switch to the other
    // one. Each direction is tried once, so two mismatches cannot ping-pong.
    //
    // 附件/图片 block 必须排在文本 block **之前**：服务端按顺序读，附件先入场，
    // 文本里只留问题本身，不放任何附件清单。
    //
    // 图片有两种引用形态，按待发项有没有 `uploadId` 分派：远端（iOS）的图已经分片
    // 上传落盘，只能按句柄引用；桌面端的图在本地盘上，交给服务端自己按路径读。
    const params = {
      threadId: id,
      input: [
        ...images.map((f) =>
          f.uploadId
            ? { type: "uploaded_image" as const, uploadId: f.uploadId }
            : { type: "image" as const, path: f.path },
        ),
        ...documents.map((f) => ({ type: "attachment" as const, path: f.path })),
        { type: "text" as const, text },
      ],
    };
    // `turn/interject` 的入参只认文本——附件会被服务端静默丢掉——所以带附件时
    // 只能走 `turn/start`，纯文本才沿用上面那次方法自愈。
    const hasFiles = files.length > 0;
    let method: "turn/interject" | "turn/start" =
      !hasFiles && store.peek(id)?.status === "running" ? "turn/interject" : "turn/start";
    // 上下文锚点（spec §7.1）：客户端声明它以为的会话世代与游标。只在 `turn/start`
    // 上发（`turn/interject` 是并入在途回合，不重开上下文），且仅手机端发；桌面端
    // 字节不变。`contentEpoch` 未知（null）时不发——缺字段=保守，服务端就不校验。
    const withAnchor = (m: "turn/interject" | "turn/start") =>
      isRemoteClient() && session.contentEpoch !== null && m === "turn/start"
        ? {
            ...params,
            contextAnchor: {
              contentEpoch: session.contentEpoch,
              lastItemId: session.lastServerItemId,
            },
          }
        : params;
    let lastError: unknown = null;
    for (let hop = 0; hop < 2; hop += 1) {
      try {
        await c.request(method, withAnchor(method));
        // 成功才清空该会话的待发附件（本地路径），服务端已把它们落库。
        setPending((prev) => ({ ...prev, [id]: [] }));
        return true;
      } catch (e) {
        // 锚点已失效（会话在别处被清空）：绝不自动重发——用户的意图不该落到一个
        // 他没看过的上下文上。置 syncing、清掉两道会挡住重同步的守卫、走全量 resume
        // （spec §7.3）。先于既有的错误处理，否则会被当成普通失败。
        if (
          isRemoteClient() &&
          (e as { data?: { code?: string } } | undefined)?.data?.code === "stale_context"
        ) {
          const v = store.peek(id);
          if (v) {
            v.syncState = "syncing";
            // `0` 只是「强制作废」的哨兵：清掉 warm 后 selectThread 会走 resume 全量
            // 回放，回放中 upsertItems 会以真实世代覆盖它。
            v.session.adoptEpoch(0, null);
          }
          warm.current.delete(id);
          inFlightResume.current.delete(id);
          store.view(id).session.lastError = "会话内容已在别处被清空，正在重新加载";
          // 发送没有成功：乐观回声必须回滚，否则转录里会留下一条「已发出」的幽灵
          // 消息（与下方的普通失败路径一致）。草稿与待发附件保留，用户确认后可重发
          //（spec §7.3「草稿保留」）。
          session.dropLocalUserMessage(text);
          force((n) => n + 1);
          void selectThread(id, { leaveBoard: false, forceResume: true });
          return false;
        }
        lastError = e;
        const code = sendMethodMismatchCode(e);
        // 带附件时没有可换的方法：换到 turn/interject 只会丢掉附件。就地失败，
        // 把错误交给用户，附件留着重试。纯文本才做那一次方法互换。
        if (hasFiles || code === null) break;
        // Adopt the server's view so the next send (and the button) is right.
        method = method === "turn/interject" ? "turn/start" : "turn/interject";
        session.turnActive = method === "turn/interject";
        const view = store.peek(id);
        if (view) view.status = method === "turn/interject" ? "running" : "idle";
      }
    }
    session.lastError = formatError(lastError);
    // Roll back the optimistic bubble so a rejected send does not leave a
    // phantom user message. (The server echoes the opening item on success, so
    // this only ever removes *our* pending copy.) The attachments stay pending:
    // the user's files must survive a failed send for a retry.
    session.dropLocalUserMessage(text);
    force((v) => v + 1);
    return false;
  };

  /**
   * 附件入口（桌面端）：一次「原生多选 → 逐个预检 → 入待发列表」。
   *
   * 文档与图片**共用一个选择器**：`filters` 给出并集（默认项）与两个收窄项，用户不必
   * 先判断文件属于哪一类；分类由路径扩展名决定（两个白名单互斥，见
   * `pendingKindFromPath`），再按类选各自的预检函数（上限不同：文档 50 MB、图片
   * 20 MB）。发送时的协议形态仍由 `kind` 决定，本入口只负责带对 `kind` 入列。
   *
   * 逐个本地预检：不合格的就地报错（写进会话错误）并丢弃，合格的才进待发列表。
   * 报错与加入互不影响——一次多选里混着好坏文件是常态，不能因为一个坏文件把
   * 整次选择作废。
   */
  const pickIntoSession = async (): Promise<void> => {
    const id = store.currentId;
    if (!id) return;
    // iOS/远端没有原生选择器：桌面端能力，在别处不假装有。远端图片走自己的路
    // （见 `pickImagesOnRemote`），同样不碰这里。
    if (isRemoteClient()) return;
    const { open } = await import("@tauri-apps/plugin-dialog");
    const picked = await open({
      directory: false,
      multiple: true,
      filters: [
        // 默认项是并集：一次选择里文档与图片都能选。
        { name: "全部支持的格式", extensions: [...ATTACHMENT_EXTENSIONS, ...IMAGE_EXTENSIONS] },
        { name: "文档", extensions: [...ATTACHMENT_EXTENSIONS] },
        { name: "图片", extensions: [...IMAGE_EXTENSIONS] },
      ],
    });
    // 多选回数组；单选/取消回字符串或 null。统一成数组再处理。
    const paths = Array.isArray(picked) ? picked : picked ? [picked] : [];
    if (paths.length === 0) return;

    const problems: string[] = [];
    const accepted: PendingAttachment[] = [];
    for (const p of paths) {
      const name = fileNameOf(p);
      // 桌面端拿不到 file size（dialog 只给路径）：大小交给服务端权威校验，
      // 这里只做扩展名预检——传 size=0 意为"未知"，不会误判为超限，避免把
      // 注定被拒的类型发出去。
      //
      // 分类先于预检：扩展名决定走哪套白名单/上限。文档与图片的白名单互斥，
      // 故一个路径只会落到其中一支。
      const kind = pendingKindFromPath(p);
      const problem = kind === "image" ? imageAttachmentProblem(p, 0) : attachmentProblem(p, 0);
      if (problem) {
        problems.push(`${name}：${problem}`);
        continue;
      }
      accepted.push({ path: p, name, size: 0, kind });
    }
    if (accepted.length > 0) {
      setPending((prev) => ({ ...prev, [id]: [...(prev[id] ?? []), ...accepted] }));
    }
    if (problems.length > 0) {
      store.view(id).session.lastError = problems.join("；");
      force((v) => v + 1);
    }
  };

  /**
   * 附件入口：桌面端打开一次原生多选（文档与图片混选，按扩展名分流）；远端
   * （iOS）没有共享文件系统，回落为图片专用选择器（那里本就不支持文档附件）。
   */
  const pickAttachments = (): Promise<void> =>
    isRemoteClient() ? pickImagesOnRemote() : pickIntoSession();

  /**
   * 远端（iOS）的图片选择器：webview 自带的 `<input type="file">`。
   *
   * 这里没有 Tauri dialog（那是桌面端能力），也不该有：iOS 上相册由系统选择器
   * 交付，`accept="image/*"` 让系统只显示图片，`multiple` 允许多选。选中的是
   * `File`（字节在本地内存里，iOS 与桌面不共享文件系统），故必须分片上传。
   *
   * 选择器是**当场造、用完即摘**的：没有任何 UI 依赖它长期存在，挂一个隐藏 input
   * 在 DOM 里只会多一份要维护的状态。
   *
   * 与 `pickIntoSession` 同一套「逐个处理、好坏互不影响」的语义：坏的（预检不过、
   * 或上传失败）就地报错并丢弃，好的照常入列；一次多选里混着好坏是常态，不能因为
   * 一个坏文件把整次选择作废。
   */
  const pickImagesOnRemote = async (): Promise<void> => {
    const id = store.currentId;
    if (!id) return;
    // 注意：`pickImageFiles()` 必须在**用户手势的同一个任务**里被调用（它内部
    // `input.click()` 才会打开系统选择器）。所以它之前不许插入任何 `await`——加了
    // 一个就会让 iOS 静默地什么都不弹。
    const files = await pickImageFiles();
    if (files.length === 0) return;

    const problems: string[] = [];
    const accepted: PendingAttachment[] = [];
    for (const file of files) {
      // 预检用本地就能知道的字节数与文件名：不合格的不必先花带宽传上去再被拒。
      //
      // HEIC/HEIF 是唯一例外——`IMAGE_EXTENSIONS` 白名单里没有它，但那条路**正是**
      // 要给 `uploadImage` 去转码的（iOS 相册默认就是 HEIC，在这里拦下等于这个功能
      // 永远走不到）。所以对 HEIC 只保留 20 MB 上限，扩展名白名单交给转码那一步：
      // 转不动就由 `uploadImage` 抛明确的 HEIC 错误。非 HEIC 仍走完整预检。
      const problem = isHeic(file)
        ? imageSizeProblem(file.size)
        : imageAttachmentProblem(file.name, file.size);
      if (problem) {
        problems.push(`${file.name}：${problem}`);
        continue;
      }
      try {
        // 上传成功即以服务端返回的**工作区相对路径**入列：`image/read` 以会话 cwd
        // 为根，只有这个路径能读回缩略图（OS 对话框给的绝对路径做不到）。
        const { uploadId, path } = await uploadImage(imageCall, id, file);
        accepted.push({ path, name: file.name, size: file.size, kind: "image", uploadId });
      } catch (e) {
        problems.push(`${file.name}：${formatError(e)}`);
      }
    }
    if (accepted.length > 0) {
      setPending((prev) => ({ ...prev, [id]: [...(prev[id] ?? []), ...accepted] }));
    }
    if (problems.length > 0) {
      store.view(id).session.lastError = problems.join("；");
      force((v) => v + 1);
    }
  };

  /** 从当前会话的待发列表里移除一个附件（用户点了 chip 上的叉）。 */
  const removePendingAttachment = (path: string): void => {
    const id = store.currentId;
    if (!id) return;
    setPending((prev) => ({ ...prev, [id]: (prev[id] ?? []).filter((a) => a.path !== path) }));
  };

  const interrupt = () => {
    const id = store.currentId;
    if (!id) return;
    clientRef.current
      ?.request<{ alreadyIdle?: boolean }>("turn/interrupt", { threadId: id })
      .then((resp) => {
        // 服务端说「本来就没在跑」：本地那枚 Stop 是过期状态（`turn/completed`
        // 在出站队列被灌满时丢了）。立刻纠正，否则按钮会一直显示 Stop，用户
        // 以为没点到，反复点也只是空转。不清 items——历史内容本身没有错。
        if (resp?.alreadyIdle) {
          store.view(id).session.clearTurn();
          force((v) => v + 1);
        }
      })
      .catch((e) => {
        store.view(id).session.lastError = formatError(e);
        force((v) => v + 1);
      });
  };

  /**
   * 把当前会话输入框里的草稿写回该会话。
   *
   * 草稿属于「那个会话」而不是「那个输入框」：每次击键都落到 store 的
   * `view(id).draft`（切走时留在原处、切回时原样出现），受控的文本框再从
   * `current.draft` 取值。store 是可变实例，改动必须经 `force` 才能抵达视图——
   * 代价与流式期间每个 delta 同量级，而昂贵的历史消息已由 `AgentMessage` /
   * `ToolCallCard` 的 memo 挡住，故无需为击键另建一套状态。
   *
   * 没有当前会话时无处可放，直接忽略（此时输入框由 `disabled` 置灰）。
   */
  const changeDraft = (text: string) => {
    const id = store.currentId;
    if (!id) return;
    store.setDraft(id, text);
    force((v) => v + 1);
  };

  /**
   * 拖入的路径落点：光标位置（不驱动渲染，故用 ref）与最近一次定位请求。
   *
   * `caret` 记用户最后一次移动光标的位置，拖入时用它决定插在哪；`seek` 是给
   * `MessageInput` 的"把光标放到这里"请求，用自增 `seq` 让同位置连拖也能各生效一次。
   */
  const composerCaret = useRef(0);
  const seekSeq = useRef(0);
  const [caretSeek, setCaretSeek] = useState<{ seq: number; pos: number } | null>(null);
  const composerCardRef = useRef<HTMLDivElement | null>(null);

  /**
   * 把拖入的路径插进**当前会话**的草稿。
   *
   * 与 `changeDraft` 同一条通路（写 `view(id).draft` + `force`）：草稿属于会话，
   * 所以切走再切回，路径仍在原会话的输入框里。没有当前会话时无处可放，忽略——
   * 与输入框的 `disabled` 护栏一致，不留"拖进去却静默丢路径"的死路。
   */
  const dropPathsIntoDraft = (paths: string[]): void => {
    const id = store.currentId;
    if (!id) return;
    const { text, caret } = insertDroppedPaths(store.view(id).draft, composerCaret.current, paths);
    store.setDraft(id, text);
    composerCaret.current = caret;
    setCaretSeek({ seq: ++seekSeq.current, pos: caret });
    force((v) => v + 1);
  };

  const dragDrop = useDragDropPaths({
    subscribe: subscribeDragDrop,
    isRemote: isRemoteClient(),
    targetRef: composerCardRef,
    onDrop: dropPathsIntoDraft,
  });

  /**
   * 记下某会话下拉写入成功后的模型结果。
   *
   * 只更新**那个**会话的 `info.model_ref` / `info.model`：写的是哪个会话，就更新
   * 哪个——切换会话的瞬间下拉可能已不指向当前会话，按 id 落笔才不会把结果写到
   * 别人头上。写入方（ModelPicker）已确认 RPC 成功，这里不做乐观更新。
   */
  const changeThreadModel = (id: string, ref: string | null, model: string | null) => {
    const view = store.peek(id);
    if (!view) return;
    if (view.info) view.info = { ...view.info, model_ref: ref, model: model ?? view.info.model };
    force((v) => v + 1);
  };

  /**
   * 记下某会话 effort 控件写入成功后的**已应用**档位（`null` = 已清除覆盖）。
   *
   * 与 `changeThreadModel` 同理：只更新被点名的那条会话，写入在途时用户可能已切走，
   * 按 id 落笔才不会落到别人头上。写入方（ModelPicker）已确认 RPC 成功、并以服务端
   * 回包为准，这里不再乐观推断。跨客户端改的走 `thread/reasoningEffortChanged`。
   */
  const changeThreadEffort = (id: string, effort: string | null) => {
    const view = store.peek(id);
    if (!view) return;
    view.reasoningEffort = effort;
    force((v) => v + 1);
  };

  /**
   * 切换宿主级「后台值守」。不做乐观更新：值以宿主返回的为准，安装/卸载失败
   * 的告知（warning）原样摆到面板上——静默会让用户以为已经生效。
   */
  const onToggleWatchman = async (next: boolean) => {
    // 先标记已触碰，再发写入（与 changeTheme 对 theme 的做法一致）：此刻若还有
    // 一趟 ui/settings/read 在途（如重启重放的握手），它回来时必须让位。
    board.watchmanTouchedRef.current = true;
    try {
      const result = await writeBoardWatchman(board.boardRpc, next);
      board.setWatchmanEnabled(next);
      board.setWatchmanWarning(result?.warning ?? null);
    } catch (error) {
      board.setWatchmanWarning(formatError(error));
    }
  };

  // Sidebar badges: server-authoritative status per thread + unread marker for
  // threads that finished a turn while the user was looking elsewhere.
  const statuses = new Map<string, ThreadStatus>();
  const unread = new Map<string, TurnStatus>();
  for (const g of sess.groups)
    for (const t of g.threads) {
      const v = store.peek(t.thread_id);
      if (!v) continue;
      statuses.set(t.thread_id, v.status);
      if (v.unread && t.thread_id !== sess.currentId)
        unread.set(t.thread_id, v.session.lastStatus ?? "completed");
    }

  const approval = current?.approval ?? null;
  const bannerItems = store
    .pendingApprovalsElsewhere()
    .filter((r) => !dismissedApprovals.current.has(r.id));

  // 首启的 iOS 没有远端配置时先配对：此时 transportFactory 会退回
  // tauriTransport()，而 iOS 上根本没有 Tauri IPC——继续往下渲染就是死路。
  // 桌面永远不会进这个分支（isIos 为 false）。
  if (needsPairing(isIos())) {
    return (
      <PairingScreen
        // PairingScreen 保证这个回调发生在 saveRemoteConfig 之后，所以此刻
        // 存储里已经有 {url, token}；版本号让上面的 effect 重跑，这一次
        // transportFactory 选中的是 ws。
        onPaired={() => prefs.setPairingVersion((v) => v + 1)}
        redeem={defaultRedeem}
      />
    );
  }

  // 已配对的手机点「重新配对」：同一个界面，多一条返回。非破坏性——提交成功
  // 才覆盖配置，返回即回原离线态继续退避重连。
  if (prefs.repairing) {
    return (
      <PairingScreen
        // 预填当前已存地址（用户可改可重扫）；读不到时留空，不影响提交。
        initialUrl={
          storedRemoteConfig(
            (globalThis.localStorage as StorageLike | undefined) ?? emptyStorageLike,
          )?.url
        }
        notice={prefs.repairNotice ?? undefined}
        onCancel={() => {
          prefs.setRepairing(false);
          prefs.setRepairNotice(null);
          // 记住"用户已经答过"：退避重连再撞 4401 也不得把他弹回来。
          prefs.repairDismissedRef.current = true;
        }}
        onPaired={() => {
          // 已配对过时 `inited.current` 仍是 true，单靠版本号会被守卫挡掉；
          // 必须先放开守卫，让旧 effect 走 cleanup、新 effect 真正建连。
          inited.current = false;
          prefs.setRepairing(false);
          prefs.setRepairNotice(null);
          prefs.setPairingVersion((v) => v + 1);
        }}
        redeem={defaultRedeem}
      />
    );
  }

  return (
    <>
      {prefs.onboarding.needed && (
        <OnboardingWizard
          reasons={prefs.onboarding.reasons}
          call={(m, p) => (clientRef.current as RpcClient).request(m, p)}
          onDone={() => {
            prefs.setOnboarding({ needed: false, reasons: [] });
            // 新配置要重启侧车才生效（cfg/清单在启动时读入）。宿主重启后会经
            // 既有 exited → 重新握手路径刷回来；顺带尽力刷一次模型清单。
            void restartSidecar();
            void modelCall("model/list", {}).catch(() => {});
          }}
        />
      )}
      <div className="app-shell flex h-screen flex-col bg-surface text-fg">
        <TitleBar />
        <div className="flex min-h-0 flex-1 flex-row">
        {isMobile && sess.sidebarOpen && (
          <div
            className="app-sidebar-backdrop"
            aria-hidden="true"
            onClick={() => sess.setSidebarOpen(false)}
          />
        )}
        {/*
         * 包裹层必须自己也是 flex 容器并带 `min-h-0`。它无条件存在（桌面端只是
         * 没有手机类名），而 `ThreadSidebar` 的 `aside` 是 flex item：若包裹层是
         * 块级容器，`aside` 会按内容取高并溢出这一行，行内随之长高、撑大整个文档，
         * 于是内层 `flex-1 overflow-y-auto` 永远不溢出（不滚动），页面本身却能上下
         * 滑动。`shrink-0` 锁住既有宽度（原先 `flex-basis:auto` 也不收缩），故桌面
         * 与手机布局逐像素不变，仅去掉溢出。
         */}
        <div
          className={
            isMobile
              ? `app-sidebar flex min-h-0 shrink-0${sess.sidebarOpen ? " sidebar-open" : ""}`
              : "flex min-h-0 shrink-0"
          }
        >
        <ThreadSidebar
          groups={sess.groups}
          workspaces={sess.workspaces}
          currentId={sess.currentId}
          pinned={sess.pinned}
          statuses={statuses}
          unread={unread}
          onSelect={selectThread}
          onRename={renameThread}
          onDelete={deleteThread}
          onTogglePin={onTogglePin}
          onSetTag={onSetTag}
          onCreateGroup={sess.createGroup}
          onUpdateGroup={sess.updateGroup}
          onSetGroupMembers={sess.setGroupMembers}
          onDeleteGroup={sess.deleteGroup}
          onError={setCurrentError}
          tagNames={tagNames}
          onReorderPinned={onReorderPinned}
          onNew={onNew}
          onRemoveWorkspace={removeWorkspace}
          onBrowse={onBrowse}
          boards={board.boards}
          boardSummaries={board.boardSummaries}
          selectedBoard={board.selectedBoard}
          onCreateBoard={(path) => void onCreateBoard(path)}
          onRemoveBoard={(path) => void onRemoveBoard(path)}
          onOpenBoard={onOpenBoard}
          isMobile={isMobile}
          statusStale={offline}
          onOpenSettings={() => prefs.setSettingsOpen(true)}
          onLinkFolder={linkFolder}
          onUnlinkFolder={unlinkFolder}
        />
        </div>
        <div className="app-main relative flex min-w-0 flex-1 flex-col">
          {isMobile && (
            <button
              type="button"
              aria-label="会话列表"
              aria-expanded={sess.sidebarOpen}
              onClick={() => sess.setSidebarOpen(true)}
              // 手机上的唯一会话入口：44px 触控目标（Apple HIG），浮在主列左上角，
              // 不再占一条常驻的左侧竖条——抽屉本身是 fixed 离屏的，收起时聊天区
              // 因此能拿满整宽。顶部让位内边距由 .app-main 提供（见 index.css），
              // 免得按钮压住首条消息或看板顶栏。
              className="absolute left-1 top-1 z-10 flex size-11 items-center justify-center rounded-md bg-panel/80 text-fg-muted hover:text-fg"
            >
              <svg viewBox="0 0 16 16" className="size-5" fill="currentColor" aria-hidden="true">
                <path d="M2 4h12v1.5H2V4Zm0 3.25h12v1.5H2v-1.5ZM2 10.5h12V12H2v-1.5Z" />
              </svg>
            </button>
          )}
          {/* 看板是主区域的一个平级页面：选中才出现，问的是被选中那个项目。
              离开看板走「点某条会话」（selectThread/newThread 清 selectedBoard）；
              没有选中时主区域就是原来的对话。 */}
          {board.selectedBoard !== null ? (
            <section
              aria-label="Superpowers 看板"
              className="flex min-h-0 flex-1 flex-col overflow-hidden border-b border-line bg-panel"
            >
              {/* 顶栏：项目路径 + 开关/值守 + 入队 + 错误行 */}
              <div className="flex items-center gap-3 px-4 pt-3">
                <span className="truncate font-mono text-xs text-fg-muted" title={board.selectedBoard}>
                  {board.selectedBoard}
                </span>
              </div>
              <SuperpowersKanbanSettings
                switchOn={board.boardOn}
                source={board.boardSource}
                onToggle={(next) => void onToggleBoardSwitch(next)}
                watchmanEnabled={board.watchmanEnabled}
                onToggleWatchman={(next) => void onToggleWatchman(next)}
                watchmanWarning={board.watchmanWarning}
              />
              {board.boardError !== null && (
                <div className="flex items-center gap-3 px-4 pb-3 text-xs text-red-400">
                  <span>{BOARD_ERROR_TEXT[board.boardError]}</span>
                </div>
              )}
              <SuperpowersKanbanEnqueue
                pickFile={pickFile}
                enqueue={(spec, plan) =>
                  enqueueBoardCard(board.boardRpc, board.selectedBoard!, spec, plan)
                }
                prequeue={(title) =>
                  enqueuePrequeuedCard(
                    board.boardRpc,
                    board.selectedBoard!,
                    title,
                    sess.currentId,
                  )
                }
              />
              <SuperpowersKanbanView
                switchOn={board.boardOn}
                source={board.boardSource}
                cards={board.boardCards}
                pluginMissing={board.boardPluginMissing}
                expandedDone={board.doneExpanded}
                onToggleDone={() => board.setDoneExpanded((v) => !v)}
                onOpenThread={(id) => {
                  // 点卡片上的会话链接 = 切到那个会话、离开看板（spec §4.7）：
                  // 看板是对话的兄弟视图，回去走侧栏，不再盖在会话上。
                  board.setSelectedBoard(null);
                  void selectThread(id);
                }}
                onPromote={(cardId) => {
                  // 提升要先补上 spec/plan（两下选择器，任一下取消就整个不作数），
                  // 再让插件下个 tick 把预排队卡翻成排队卡。
                  void (async () => {
                    board.setBoardError(null);
                    try {
                      const spec = await pickFile();
                      if (spec === null) return;
                      const plan = await pickFile();
                      if (plan === null) return;
                      await promoteBoardCard(
                        board.boardRpc,
                        board.selectedBoard!,
                        cardId,
                        spec,
                        plan,
                      );
                      board.refreshBoards();
                    } catch (error) {
                      // 与看板其它写入同一条通路：把失败翻成面板上那句可操作的说法。
                      board.setBoardError(boardErrorKind(error));
                    }
                  })();
                }}
              />
            </section>
          ) : (
            <>
              {sess.currentId !== null &&
                ((offline && cachedAt.current.has(sess.currentId)) ||
                  store.peek(sess.currentId)?.syncState === "syncing") && (
                  <CacheNotice
                    savedAt={cachedAt.current.get(sess.currentId) ?? null}
                    syncing={!offline && store.peek(sess.currentId)?.syncState === "syncing"}
                  />
                )}
              {/*
                转录区是审批弹窗的定位作用域：弹窗只盖住历史内容，不盖输入卡片。
                它曾经挂在 `.app-main`（整个主列）下，`absolute inset-0` 于是连
                Stop 一起涂黑——一个正在等审批的 turn 在用户眼里变成「点不动」，
                只能先答审批才能打断。作用域收到这一层后，弹窗的 `inset-0` 只覆盖
                `flex-1` 的转录列，输入区仍在它下面可用。
              */}
              <div className="relative flex min-h-0 min-w-0 flex-1 flex-col">
                <ChatView
                  items={current?.session.items ?? []}
                  error={current?.session.lastError ?? null}
                  retrying={current?.session.retrying ?? null}
                  threadId={sess.currentId}
                  // 稳定的 `imageCall`（见上），别在这里内联箭头：它会进
                  // `useImageData` 的依赖数组，换身份即等于每次渲染重拉图片。
                  call={imageCall}
                  structuralVersion={current?.session.structuralVersion ?? 0}
                />
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
              <MessageInput
                turnActive={current?.session.turnActive ?? false}
                onSend={send}
                onInterrupt={interrupt}
                mode={current?.mode ?? null}
                onModeChange={setThreadMode}
                onSlashCommand={(name, args) => void onSlashCommand(name, args)}
                value={current?.draft ?? ""}
                onDraftChange={changeDraft}
                cardRef={composerCardRef}
                dropActive={dragDrop.active}
                caretSeek={caretSeek}
                onCaretChange={(caret) => {
                  composerCaret.current = caret;
                }}
                // 待发附件按会话取：切会话时 chip 行随当前会话变，绝不会串。
                attachments={sess.currentId ? (pending[sess.currentId] ?? []) : []}
                onPickAttachments={() => void pickAttachments()}
                onRemoveAttachment={removePendingAttachment}
                threadId={sess.currentId}
                // 稳定的 `imageCall`（见上）：待发图片 chip 的缩略图也经它读取，
                // 内联箭头会让每张缩略图每次渲染都重拉。
                call={imageCall}
                disabled={current === null}
                isMobile={isMobile}
                modelPicker={
                  current !== null && sess.currentId !== null ? (
                    <ModelPicker
                      call={modelCall}
                      threadId={sess.currentId}
                      currentRef={current.info?.model_ref ?? null}
                      currentModel={current.info?.model ?? null}
                      currentEffort={current.reasoningEffort}
                      onChanged={(id, ref, model) => changeThreadModel(id, ref, model)}
                      onEffortChanged={(id, effort) => changeThreadEffort(id, effort)}
                    />
                  ) : null
                }
              />
            </>
          )}
          <ApprovalBanner
            items={bannerItems}
            onJump={(id) => void selectThread(id)}
            onDismiss={() => {
              for (const r of bannerItems) dismissedApprovals.current.add(r.id);
              force((v) => v + 1);
            }}
          />
          {/* StatusBar 保持无条件渲染：看板只覆盖会话区，状态栏是窗口级的。 */}
          <StatusBar
            cwd={current?.info?.cwd ?? null}
            status={status}
            // 手机端离线时给出重连的出路：状态栏此刻必然在屏上，与状态显示同源。
            onRepair={
              offline
                ? () => {
                    // 手动进入不带自动路径的原因文案（那是另一条路的事）。
                    prefs.setRepairNotice(null);
                    prefs.setRepairing(true);
                  }
                : undefined
            }
            usage={current?.session.usage ?? null}
            // 切换会话时数字直接跳终值，不从上一个会话滚过来。
            usageId={sess.currentId}
            /*
             * 右侧详情栏（子 agent + Git Diff）的**常驻**入口：收起动作够不着的
             * 地方必须留一条回去的路，否则收起就成了单向操作。常驻图标在收起后
             * 仍在，故栏的开合可逆（面板的「关闭」是另一条路，两者都走 panelOpen）。
             * 手机端栏是抽屉、由媒体查询收口，入口不适用（与侧栏一致）。
             * 标签用「会话详情面板」而非「收起/展开…」，以免与控件内文案的无障碍名
             * 撞车（两个同名控件会让人/测试都选不准）。
             */
            actions={
              sess.currentId && !isMobile ? (
                <button
                  type="button"
                  aria-label="会话详情面板"
                  aria-expanded={sess.panelOpen}
                  title={sess.panelOpen ? "收起会话详情" : "展开会话详情"}
                  onClick={() => {
                    sess.setPanelOpen((v) => !v);
                    // 每次打开都拿一份当前 diff：面板可能开着很久，快照会过期。
                    if (!sess.panelOpen) void diff.loadThreadDiff(sess.currentId!, diff.diffBase);
                  }}
                  className={`flex items-center gap-1 rounded p-1 hover:bg-raised/50 hover:text-fg ${
                    sess.railRows.length > 0 ? "text-fg-muted" : "text-fg-subtle"
                  } ${sess.panelOpen ? "bg-raised/40" : ""}`}
                >
                  <svg viewBox="0 0 16 16" className="size-4" fill="currentColor" aria-hidden="true">
                    <path d="M1.5 3.5h5.2v9H1.5v-9Zm1.2 1.2v6.6h2.8V4.7H2.7Z" />
                    <path d="M9.1 5.8 11.3 8 9.1 10.2V5.8Z" />
                    <path d="M12.2 5.8 14.4 8l-2.2 2.2V5.8Z" />
                  </svg>
                  {sess.railRows.length > 0 && <span className="text-xs">{sess.railRows.length}</span>}
                </button>
              ) : null
            }
          />
        </div>
        {/*
          右侧详情栏：`[子 agent | Git Diff]`，与左侧会话侧栏同级的独立列。
          它取代了此前的两处——底部的详情面板、以及独立的子 agent 栏；子 agent 的
          卡片列表与轨迹详情都收在这个栏的「子 agent」Tab 内。
          可见性由状态栏那颗图标管（`panelOpen`）；手机端不渲染（窄屏上它会挤没
          聊天区，与侧栏同一理由）。
        */}
        {sess.currentId && sess.panelOpen && !isMobile && (
          <ThreadDetailPanel
            tab={sess.detailTab}
            onTabChange={(t) => {
              sess.setDetailTab(t);
              // 切到 diff 就取一份当前快照；轨迹侧的行由 openDetail/watch 维护。
              if (t === "diff") void diff.loadThreadDiff(sess.currentId!, diff.diffBase);
              // 切走进程 tab 即离开当前进程详情:留着会让详情盖住别的 tab 的列表。
              if (t !== "processes") {
                proc.setOpenProcessId(null);
                proc.setProcessOutput(null);
              }
            }}
            onClose={() => {
              sess.setPanelOpen(false);
              void sess.closeDetail(sess.currentId!);
            }}
            railRows={railStore.get(sess.currentId)}
            selectedTaskId={sess.openSubagent}
            onOpenTask={(taskId) => void sess.openDetail(sess.currentId!, taskId)}
            onLeaveTask={() => void sess.closeDetail(sess.currentId!)}
            traceProps={
              sess.openSubagent
                ? {
                    taskId: sess.openSubagent,
                    row: railStore.get(sess.currentId).find((r) => r.taskId === sess.openSubagent) ?? null,
                    children: childrenOf(railStore.get(sess.currentId), sess.openSubagent),
                    rows: sess.traceRows,
                    onDrill: (taskId) => void sess.openDetail(sess.currentId!, taskId),
                    onMessage: async (taskId, message) => {
                      await clientRef.current?.request("agent/message", {
                        threadId: sess.currentId,
                        taskId,
                        message,
                      });
                    },
                    onCancel: async (taskId) =>
                      await clientRef.current!.request<AgentCancelPreviewResult>(
                        "agent/cancel/preview",
                        { threadId: sess.currentId, taskId },
                      ),
                    onConfirmCancel: async (taskId, token) => {
                      await clientRef.current?.request("agent/cancel", {
                        threadId: sess.currentId,
                        taskId,
                        confirmationToken: token,
                      });
                    },
                  }
                : null
            }
            diffProps={{
              // 只为**本会话**持有的快照发画面（`diffOwnerRef` 是判据，不是
              // `currentId` 的影子）。在途响应回来时人可能已经切走，届时即使
              // state 里还留着上一条，也不许它冒充当前会话的 diff。
              diff: diff.diffOwnerRef.current === sess.currentId ? diff.threadDiff : null,
              loading: diff.diffLoading,
              // 错误是**具名**的：服务端文案里没有会话标识，错了会话谁都不知道，
              // 故与快照同门——非本会话的错误宁可不显示。
              error: diff.diffOwnerRef.current === sess.currentId ? diff.diffError : null,
              activeCommit: diff.activeCommit,
              note: diff.diffNote,
              // 帧与快照同门：非本会话的帧不许冒充当前视图（`viewOwnerRef` 是判据）。
              frame: diff.viewOwnerRef.current === sess.currentId ? diff.viewFrame : null,
              onRefresh: () => {
                // 刷新要两样都重取：帧重跑模型那条命令，快照重算 commits/files。
                void diff.loadViewFrame(sess.currentId!, true);
                void diff.loadThreadDiff(sess.currentId!, diff.diffBase);
              },
              onOpenCommit: (sha) => void diff.openCommit(sess.currentId!, sha),
              onCloseCommit: () => diff.setActiveCommit(null),
              // D1 单文件懒加载：正文缺席时按 path 取一次正文并并进快照。
              onLoadFile: (path) => void diff.loadFile(sess.currentId!, path),
            }}
            processProps={{
              processes: proc.processes,
              selectedProcessId: proc.openProcessId,
              onOpenProcess: (processId) => {
                proc.setOpenProcessId(processId);
                proc.setProcessOutput(null);
              },
              onCloseProcess: () => {
                proc.setOpenProcessId(null);
                proc.setProcessOutput(null);
              },
              output: proc.processOutput,
              onKill: async () => {
                try {
                  await clientRef.current?.request("process/kill", {
                    thread_id: sess.currentId,
                    process_id: proc.openProcessId,
                  });
                } finally {
                  // 无论成败都重拉:失败时列表会保持原状态,不留下假象。
                  await proc.refreshProcesses(sess.currentId!);
                }
              },
            }}
            isMobile={isMobile}
          />
        )}
        </div>
      </div>
      <SettingsDialog
        open={prefs.settingsOpen}
        // 手机端把设置改成全屏页（见 index.css 的 [data-mobile="true"] .settings-shell）。
        // 复用上面已有的 `isMobile`，不在这里新探测。
        isMobile={isMobile}
        theme={prefs.theme}
        onThemeChange={prefs.changeTheme}
        onClose={() => prefs.setSettingsOpen(false)}
        // 远程访问 Tab 走**当前 transport**（桌面 stdio = Admin），所以能调
        // pair/create、device/revoke；网络客户端会拿到 -32014，Tab 会提示权限。
        remoteCall={(method, params) =>
          (clientRef.current as RpcClient).request(method, params)
        }
        // 插件 Tab 同样走当前 transport（桌面 stdio = Admin），能调
        // plugins/list 与 plugin/settings/*。用稳定的 `pluginCall`（见上），
        // 别在这里内联箭头——否则每次 App 渲染都会换新身份，重跑面板的读取。
        pluginCall={pluginCall}
        // 模型 Tab 同样走当前 transport（桌面 stdio = Admin），能调 model/*。
        // 用稳定的 `modelCall`（见上），别在这里内联箭头。
        modelCall={modelCall}
        // 技能三层根同样按项目解析（`<项目>/.yi-agent/skills`），候选与默认值直接
        // 复用插件那份——两者指的都是「项目目录下的 .yi-agent」，来源分叉会让用户
        // 在两个面板里看到不同的项目列表。
        skillsCall={skillsCall}
        skillsProjects={pluginProjectPaths(sess.workspaces, board.boards)}
        skillsDefaultProject={current?.info?.cwd}
        // MCP 的配置也是项目级文件（<项目>/.yi-agent/mcp.json），候选与默认值直接
        // 复用插件/技能那份——三者指的都是「项目目录下的 .yi-agent」，来源分叉会让
        // 用户在三个面板里看到不同的项目列表。
        mcpCall={mcpCall}
        mcpProjects={pluginProjectPaths(sess.workspaces, board.boards)}
        mcpDefaultProject={current?.info?.cwd}
        // 插件清单按项目安装，桌面侧车 app-server 的 workdir 却是 home，所以设置页
        // 必须把「要配置的项目」传下去。候选 = 最近目录 + 已登记看板；默认 = 当前
        // 对话的工作目录（没有对话时由面板回落到第一个候选，仍空则回落宿主 workdir）。
        pluginProjects={pluginProjectPaths(sess.workspaces, board.boards)}
        pluginDefaultProject={current?.info?.cwd}
        relayUrl={prefs.relayUrl}
        // 桌面（当前 transport 有侧车及其宿主）才给保存接缝；iOS 是个 ws 客户端。
        saveRelayUrl={isRemoteClient() ? undefined : prefs.saveRelayUrl}
        tagNames={tagNames}
        saveTagName={saveTagName}
        // `null`（尚未读到）按 undefined 传：Tab 不出现，读回来是 dev 才出现。
        buildKind={buildKind ?? undefined}
        flagsCall={flagsCall}
      />
    </>
  );
}
