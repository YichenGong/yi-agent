import type { RpcClient } from "../lib/rpc";
import { LOCAL_SLOT, type Slot } from "../lib/slots";
import type { ThreadStore } from "../lib/threadStore";
import type { useSettingsState } from "./useSettingsState";
import type { useSessionState } from "./useSessionState";
import type { useDiffState } from "./useDiffState";
import type { useProcessState } from "./useProcessState";
import type { SubagentRailStore } from "../lib/subagents";
import { SubscriptionWindow } from "../lib/subscriptionWindow";
import { ownsThreadView } from "../lib/slotRouting";
import {
  SideConversationRouter,
  isSideNotification,
  type SideConversationStore,
} from "../lib/sideConversation";
import type { SideNotification } from "../lib/sideConversation";
import { parseTheme } from "../lib/theme";
import { notifyModelCapabilitiesChanged } from "../lib/modelCapabilitiesBus";
import { notifyRuntimeSettingsInvalidated } from "../lib/runtimeSettingsBus";
import type { ReasoningCapabilities } from "../lib/models";
import { applyTagChange, applyTitleChange } from "../lib/threadListUpdate";
import { resolveTagNames } from "../lib/threadTags";
import { isRemoteClient } from "../lib/platform";
import { REPAIR_REMOVED_TEXT } from "../lib/repairCopy";
import type { SubagentTraceController } from "./useSubagentTrace";

type SettingsState = ReturnType<typeof useSettingsState>;
type SessionState = ReturnType<typeof useSessionState>;
type DiffState = ReturnType<typeof useDiffState>;
type ProcessState = ReturnType<typeof useProcessState>;

/**
 * 通知/审批/状态回调的**全部**接线（原 `App.tsx` 巨型 effect 内 `wireClient`，
 * 221 行，L1727–L1945）。
 *
 * 这里叫「工厂」而不是 hook，差别只在**取值时机**：原代码是
 *
 * ```ts
 * useEffect(() => { const wireClient = (...) => {...}; ... }, [prefs.pairingVersion]);
 * ```
 *
 * 闭包抓到的是**这次 effect 运行那一帧**的 `sess`/`prefs`/`store`。若改成
 * `useMemo`/`useCallback` 形状的 hook，依赖要么写不全（抓旧值，静默错误），
 * 要么写全（每次渲染换新身份，把下游 effect 拖进重跑）。故保留「调用方在
 * effect 里现建」的形式：依赖由参数显式传入，`tsc` 保证一个不漏。
 */
export interface NotificationRouterDeps {
  activeSlotRef: React.MutableRefObject<Slot>;
  sess: SessionState;
  prefs: SettingsState;
  diff: DiffState;
  proc: ProcessState;
  railStore: SubagentRailStore;
  store: ThreadStore;
  subWindow: React.MutableRefObject<SubscriptionWindow>;
  toppedUp: React.MutableRefObject<Set<string>>;
  warm: React.MutableRefObject<Set<string>>;
  setStatus: (v: string) => void;
  setTagNames: (v: Record<"red" | "orange" | "yellow" | "green" | "blue" | "purple", string>) => void;
  force: (updater: (v: number) => number) => void;
  markThreadRead: (id: string) => void;
  schedulePersist: (id: string) => void;
  selectThread: (id: string, opts?: { forceResume?: boolean }) => Promise<void>;
  /**
   * 该 thread 收到了活动（任意帧），用来给 resume 的停滞检测续命。
   * 只在 `ownsThreadView` 判定之后调用——非活动槽位的帧不写本视图，用它们
   * 续命会掩盖「当前会话真的卡住」的事实（设计 §4.2）。
   */
  notifySyncActivity: (threadId: string) => void;
  /** Targeted, request-correlated transcript completion; runtime may still be preparing. */
  onHistoryReady?: (client: RpcClient, threadId: string, token: string) => void;
  disposed: boolean;
  scheduleReconnect: () => void;
  /**
   * 旁支的视图 store 与路由。
   *
   * 传进来的必须是**这一个 effect** 建的那一份：绑定按连接身份记，换了 effect
   * 就意味着换了一轮连接。缺省不给会让旁支帧被静默丢弃，所以它是必填的。
   */
  sideStore: SideConversationStore;
  /**
   * 把一帧旁支通知交给对账控制器。
   *
   * 为什么不直接 `sideStore.apply`：帧必须与**分页快照**对齐（连续 seq、
   * 缺号重对账、缓存上限）。直接写 store 会得到一份「看起来合理、实际缺了
   * 一段」的正文，而这正是旁支最难被用户描述的那类故障。
   */
  acceptSideNotification: (client: RpcClient, slot: Slot, n: SideNotification) => void;
  /**
   * 子 agent 轨迹的唯一入口。
   *
   * 通知带着**送它来的那条连接**，所以归属判定在控制器里按 client 身份做，而不是
   * 读「当前打开的任务」。后者是一份 React state 的影子，漏同步一次就会把别的
   * 任务的帧画进这一条视图。
   */
  trace: SubagentTraceController;
  /**
   * Tab 的唯一入口。
   *
   * 直接调 `sess.setDetailTab` 会漏掉「离开子 agent Tab 先失效预览」这一步，
   * 于是模型推来的 `ui/gitDiff/focus` 与用户点击会得到两种行为。所有生产调用点
   * 都走这里，Tab 的副作用只有一处。
   */
  changeDetailTab: (tab: "subagents" | "diff" | "processes") => void;
}

export function createNotificationRouter(deps: NotificationRouterDeps) {
  const {
    activeSlotRef,
    sess,
    prefs,
    diff,
    proc,
    railStore,
    store,
    subWindow,
    toppedUp,
    warm,
    setStatus,
    setTagNames,
    force,
    markThreadRead,
    notifySyncActivity,
    schedulePersist,
    disposed,
    scheduleReconnect,
    sideStore,
    acceptSideNotification,
    trace,
    changeDetailTab,
  } = deps;

  const sideRouter = new SideConversationRouter(sideStore);

  return (client: RpcClient, slot: Slot = LOCAL_SLOT) => {
      client.onNotification((n) => {
        // 旁支帧先按**连接身份**路由，再做视图归属判定。
        //
        // 顺序反了会有具体后果：`ownsThreadView` 只让活动槽位写视图，而旁支
        // 的归属是「哪条连接送的」。先判归属会把非活动槽位上的旁支帧整条丢掉，
        // 用户切回那个项目时看到的是一份停住的旧视图——而这条连接明明还活着。
        //
        // `side/*` 分支必须**立即 return**：这些帧不属于任何普通会话，喂给
        // `ThreadStore.applyNotification` 会把它当成未知帧而静默无效果，更糟
        // 的是后续的 `schedulePersist` / `markThreadRead` 会把旁支活动记账到
        // 父会话上（父的未读点、内容落盘都被旁支污染）。
        if (isSideNotification(n)) {
          const binding = sideRouter.bindingFor(client, slot);
          // 未握手时没有任何绑定：帧无处可归，丢弃而不是猜一条「当前连接」。
          if (binding) acceptSideNotification(client, slot, n);
          return;
        }
        if (n.method === "settings/changed") {
          // 全局设置通知**没有** thread_id，所以必须在会话过滤之前处理：
          // `ownsThreadView` 会把它当成「不属于活动槽位的会话帧」丢掉，设置页
          // 就再也收不到刷新提示。
          //
          // 按 client 身份路由（而不是全局广播）：桌面可以同时连着多个宿主，
          // 项目 A 的保存不能去刷新另一台主机 B 的设置页。通知只带 revision 和
          // 键名，不缓存值——订阅方自己去 `settings/read`。
          notifyRuntimeSettingsInvalidated(client);
          return;
        }
        // 非活动槽位（别的远端项目、或本地而用户正看远端）的通知**不写**本视图。
        //
        // spec §4.6 判据 17：A 项目跑一个多步任务时，B 项目的会话视图里不得
        // 出现 A 的 `turn/*`。所有槽位共用同一个 `ThreadStore`，而 thread id 在
        // 各 app-server 之间没有任何全局唯一性保证——两个项目里都叫 `t-1` 完全
        // 可能。放进来不是「多一行」，而是把两条对话的内容拼进同一份视图。
        //
        // 只丢弃**视图写入**这一层：连接本身仍要收帧，重连/握手/状态记账照常，
        // 否则切到该槽位时会发现它的连接已经半死。
        if (!ownsThreadView(slot, activeSlotRef.current, n.method)) {
          return;
        }
        // 回放帧就是「活动」：续一次命。放在 ownsThreadView 之后是刻意的——
        // 非活动槽位的帧不会写本视图，用它们续命会掩盖「当前会话真的卡住」。
        {
          const params = n.params as { thread_id?: unknown } | null | undefined;
          if (typeof params?.thread_id === "string") {
            notifySyncActivity(params.thread_id);
          }
        }
        if (n.method === "thread/historyReady") {
          deps.onHistoryReady?.(client, n.params.thread_id, n.params.request_token);
          return;
        }
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
          !sess.pinnedRef.current.some(
            (t) => t.thread_id === n.params.thread_id,
          )
        ) {
          void sess.refreshThreads();
        }
        // 子 agent 的通知不进 ThreadStore:它们是对话的附属视图,不是对话本身。
        if (n.method === "agent/children/updated") {
          railStore.applyNotification(n.params.threadId, n.params.children);
          // 详情头部的状态徽章读的是同一份行：列表刷新后要把这条任务的最新
          // 状态交给动作记录，否则任务已经失败，头部还停在「运行中」。
          if (trace.getSnapshot().selected !== null) {
            const selected = trace.getSnapshot().selected!;
            trace.updateTask(
              selected,
              railStore
                .get(selected.threadId)
                .find((row) => row.taskId === selected.taskId) ?? null,
            );
          }
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
              (n.params.reasoning_capabilities as
                ReasoningCapabilities | null | undefined) ?? null,
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
          changeDetailTab("diff");
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
          // 帧只在**它所属的那条连接**仍持有该任务的订阅时才落地；控制器按
          // (slot, 服务端绑定代际, thread, task) 逐项核对，换任务/换会话/换连接
          // 之后的迟到帧一律丢弃，而不是拼进眼前这条轨迹。
          const source = trace.connectionFor(client, slot);
          if (source === null) return;
          trace.accept(source, n.params);
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
        if (n.method === "turn/completed") {
          // 用户正看着的这个会话里跑完了一轮：内容他就读在眼前，本地不会亮蓝点，
          // 但**服务端水位还停在这一轮之前**。不上报的话，重连/刷新后
          // `thread/listAll` 会按旧水位把蓝点算出来——即「读过的东西又变成未读」。
          // 只对当前会话这么做：后台会话跑完理应保持未读，等信息被看到再清。
          if (n.params.thread_id === sess.currentIdRef.current) {
            markThreadRead(n.params.thread_id);
          }
        }
        // `Notification` 是判别联合：只有带 `thread_id` 的通知才有会话内容可落盘
        // （`error` 无归属、由 store 归到当前会话，在此跳过）。其余 `agent/*`、
        // `ui/*` 通知已在上方分支提前返回。
        //
        // `thread/readStateChanged` 是唯一能走到这里的纯元数据通知（标题/标签
        // 都在上方提前返回）：它只带两个时间戳，没有会话内容可落。给它排一次
        // 内容落盘纯属白写一次 IndexedDB。
        if ("thread_id" in n.params && n.method !== "thread/readStateChanged") {
          schedulePersist(n.params.thread_id);
        }
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
          // 先让轨迹知道这条连接没了：它的在途 watch 与预览都随连接作废，
          // 但已读到的历史与草稿留着——用户没走，等重连后手动 retry 即可。
          const traceSource = trace.connectionFor(client, slot);
          if (traceSource !== null) trace.disconnected(traceSource);
          // 这个客户端已经死了：立刻注销它的 transport 监听。Tauri 的
          // `app-server://message` 是全局事件名，宿主拉起新侧车后，陈旧监听照样
          // 会收到新进程的帧——不摘掉就会与重连后的新客户端各应用一遍（逐字流
          // 重复，直到 item/completed 按 id 整条替换才"收敛"）。
          sideRouter.disconnected(sideRouter.bindingFor(client, slot));
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
}
