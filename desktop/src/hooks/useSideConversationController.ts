import { useCallback, useRef, useState } from "react";
import type { RpcClient } from "../lib/rpc";
import { formatError } from "../lib/errorMessage";
import type { SideSummaryWire } from "../lib/generated/SideSummaryWire";
import type {
  SideConversationStore,
  SideHostBinding,
  SideHostKey,
  SideSubmitReceipt,
  SideTarget,
} from "../lib/sideConversation";
import type { SideSyncController } from "../lib/sideSync";
import type { SideExitCoordinator } from "../lib/sideExit";

/**
 * 「当前正在看哪个会话」。
 *
 * `main` 与 `side` 共用同一套视图位置，但**父身份永远在**：旁支的所有操作都要
 * 回到父，且切回父时父的草稿/附件/滚动位置必须原样还在（它们从没被旁支改过）。
 *
 * 为什么不复用 `ThreadStore.currentId` 表达"选中项"：那会让旁支变成一条普通
 * 会话，从而进缓存、进搜索、进未读。这里刻意只记录一个指针。
 */
export type ActiveConversation =
  | { kind: "main"; hostKey: SideHostKey; parentId: string }
  | {
      kind: "side";
      hostKey: SideHostKey;
      parentId: string;
      sideId: string;
      generation: number;
    };

export interface SideConversationControllerDeps {
  store: SideConversationStore;
  /**
   * 退出意图账本（Task 10）。
   *
   * 控制器**只**记录意图，不负责补发：补发要在「连接恢复」时由应用 owner
   * 触发（`exits.flush(binding)`），因为只有它知道哪条连接刚刚重新握手。把
   * 重试塞进控制器会让它自带一个重试队列，而那正是 spec §6.4 明确不要的。
   */
  exits: SideExitCoordinator;
  /** 取（必要时创建）某宿主上某个父的对账控制器。 */
  controllerFor: (binding: SideHostBinding, parentId: string) => SideSyncController;
  /** store 变化后的重渲染。 */
  onChange: () => void;
  /** 生成去重用的规范 UUID（测试可注入，避免断言依赖随机值）。 */
  newRequestId?: () => string;
}

/**
 * 为什么离开。
 *
 * 三种都要销毁，差别只在给用户的说法与是否需要回父视图：
 * - `back`：用户按了返回/`/back`。回到主视图。
 * - `navigation`：切到另一条普通会话、另一个项目/宿主。视图由导航自己接管。
 * - `close`：`/side-close` 显式关闭（在主视图里关后台那条）。
 *
 * 另有两个不出现在用户操作里的：`unmount`（应用 owner 的延迟终结检查）与
 * `flag`（本机实验开关被关掉）。它们同样走销毁，只是提示文案不同。
 */
export type LeaveReason =
  | "back"
  | "navigation"
  | "close"
  | "unmount"
  | "flag";

/**
 * 本次停留（entry）。
 *
 * 「停留」是这次进入的**全部**本地状态的容器：入场票据、捕获的宿主绑定、父、
 * 首问与可选的摘要。它存在的理由只有一个——退出时所有字段都**从这份记录**取，
 * 而不是从「此刻选中的会话」推断。推断会在两种情况下出错：
 *
 * - 用户已经切到另一条会话：`store.currentId` 是新那条，退出会打到别人身上。
 * - 用户切了项目/宿主：`activeSlot` 变了，票据属于旧宿主，新连接上没有它。
 *
 * 首问只在**退出前**保留：退出立刻清空（spec §4.3「丢弃旧首问，不补发」）。
 */
interface SideEntry {
  binding: SideHostBinding;
  parentId: string;
  /** 宿主签发的入场票据；`null` = prepare 还没回。 */
  target: { hostInstanceId: string; parentThreadId: string; entryId: string } | null;
  /** 服务端确认后的这一代；`null` = 还没 open 成功。 */
  summary: { sideId: string; generation: number } | null;
  /** 入场首问。退出时清空，绝不补发。 */
  question: string | null;
}

export interface SideConversationController {
  active: ActiveConversation | null;
  /** 打开（或续用）某个父上的旁支。返回是否成功受理。 */
  open: (binding: SideHostBinding, parentId: string, question: string) => Promise<boolean>;
  /**
   * 离开旁支：**退出即销毁**（spec §4.3）。
   *
   * 这是唯一的退出口。`back`（顶栏按钮 / `/back`）、`selectMain`（切到另一条
   * 普通会话）、项目/宿主切换、以及 `/side-close` 的兼容入口全部落在这里——
   * 分支散成两三条时，总有一条会忘记录退出意图，于是用户返回了、环境还在跑。
   *
   * 立刻做的是本地部分：清掉本次停留（含首问）、丢正文与草稿、回到主视图。
   * 网络部分是异步的、不阻塞导航：`exits.flush` 在内部吸收错误并记录收据。
   */
  leaveSide: (reason: LeaveReason) => void;
  /** `/back` 的兼容别名（等价 `leaveSide("back")`）。 */
  back: () => void;
  /** 用户在主视图里显式切到某个父（真实父/宿主导航）。 */
  selectMain: (binding: SideHostBinding, parentId: string) => void;
  /** 进入某个已存在的旁支（Observe 也能用：list 里看到的条目）。 */
  enter: (
    binding: SideHostBinding,
    parentId: string,
    sideId: string,
    generation: number,
  ) => void;
  /**
   * 提交当前旁支。
   *
   * 返回三态收据（Task 2）：调用方据此决定清稿、还稿还是提示「待确认」。没有
   * 打开中的旁支时返回一条 `rejected`，且**不生成 requestId**——请求根本不存在。
   */
  submit: (text: string) => Promise<SideSubmitReceipt>;
  stop: () => Promise<void>;
  /**
   * `/side-close` 的兼容入口：在主视图里关掉**后台**那条旁支。
   *
   * 它同样是销毁（`leaveSide("close")`），只是出发时不在旁支视图里，所以
   * 需要显式给出父身份。找不到本次停留、也没有本地视图时只留一条提示。
   */
  closeBackground: (binding: SideHostBinding, parentId: string) => Promise<void>;
  /** 正在处理一条旁支 RPC（按钮据此禁用）。 */
  busy: boolean;
  /**
   * 最近一次旁支操作的失败原因，**连同它的来源身份**。
   *
   * 不提供「裸全局 error」正是为了 task 要防的那件事：A 宿主上的一次失败不能
   * 显示成 B 会话的错误横幅。渲染方用 `visibleError`（只在来源仍是当前显示的
   * 旁支时才返回它）或按 `errorTarget` 自行判断。
   */
  error: string | null;
  errorTarget: SideTarget | null;
  /** 仅当失败来源就是当前显示的旁支时为非 null。 */
  visibleError: string | null;
  /**
   * 一条「操作完成了，但本来就没有可做的事」的提示（例如没有可关闭的旁问）。
   *
   * 它不是错误：渲染方可以把它显示在中性位置。任何新的用户动作都会清掉它。
   */
  notice: string | null;
  /** 清掉错误（用户已看到）。 */
  clearError: () => void;
}

/** 两条旁支是不是同一条（只有 kind 为 side 的才有意义）。 */
function sameTarget(a: SideTarget, b: SideTarget): boolean {
  return (
    a.hostKey === b.hostKey &&
    a.parentId === b.parentId &&
    a.sideId === b.sideId &&
    a.generation === b.generation
  );
}

function defaultRequestId(): string {
  const c = globalThis.crypto;
  if (c && typeof c.randomUUID === "function") return c.randomUUID();
  // 兜底只为测试环境存在；服务端校验 UUID 形状，所以必须凑够 36 字符。
  const hex = Math.random().toString(16).slice(2).padEnd(32, "0");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-4${hex.slice(13, 16)}-a${hex.slice(17, 20)}-${hex.slice(20, 32)}`;
}

/**
 * 旁支的 RPC 动作。**所有异步操作在调用时捕获目标**。
 *
 * 这是这个 hook 里最重要的一条：`await` 之后再去读 `active` 或全局 "当前连接"
 * 都可能已经变了（用户切走了、重连换了 client）。于是每个动作先把 `sideId` /
 * `generation` / `client` 抓进局部量，回包时只认那一份。否则一次迟到的
 * `turnStart` 回包会清掉另一条会话的草稿，或者把 Stop 打到父会话上。
 */
export function useSideConversationController(
  deps: SideConversationControllerDeps,
): SideConversationController {
  const { store, controllerFor, onChange, exits } = deps;
  const newRequestId = deps.newRequestId ?? defaultRequestId;
  const [active, setActive] = useState<ActiveConversation | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  /** 一条不含错误语义的提示（没有可关闭的旁问等）。 */
  const [notice, setNotice] = useState<string | null>(null);
  /** `error` 属于哪条旁支。`null` 表示来源未知（例如根本没有目标可指）。 */
  const [errorTarget, setErrorTarget] = useState<SideTarget | null>(null);
  /**
   * 最近一次被记录的错误，供后续调用补上来源（例如 `run` 抛错时还不知道是
   * 哪条旁支，`open`/`stop` 随后补上目标）。
   */
  const lastErrorRef = useRef<{ message: string; target: SideTarget | null } | null>(null);
  /**
   * 带来源地记录一次失败。同一身份内连续失败只保留最后一条；**别的**会话的
   * 失败不覆盖这里，因为那会把 A 的原因说成 B 的。
   */
  const recordError = useCallback((message: string, target: SideTarget | null) => {
    setError(message);
    setErrorTarget(target);
    setNotice(null);
    lastErrorRef.current = { message, target };
  }, []);
  /** 记一条「没有可做的事」的提示。它是幂等的：重复触发只留一条。 */
  const recordNotice = useCallback((message: string) => {
    setError(null);
    setErrorTarget(null);
    setNotice(message);
  }, []);
  const clearScopedError = useCallback((target: SideTarget) => {
    setErrorTarget((prev) => {
      if (prev !== null && sameTarget(prev, target)) setError(null);
      return prev !== null && sameTarget(prev, target) ? null : prev;
    });
    // 这条旁支刚刚成功了一次：上一条「没有可做的事」的提示已经过时。
    setNotice(null);
  }, []);
  // 异步动作在 await 之后需要读到"最新"的 active，但读取必须显式，不能让它
  // 在闭包里悄悄变旧。这里同时保留一份 ref 影子。
  const activeRef = useRef<ActiveConversation | null>(null);
  activeRef.current = active;
  /**
   * 「用户意图」的修订号。
   *
   * 每次**用户**导航（open / enter / back / selectMain）加一。异步动作在 await
   * 之后用它判断「我出发时那个意图还算不算数」：一次迟到的 `side/open` 回包不能
   * 把已经返回的用户重新拽进旁支（spec §5.2）。后台通知不调用这些方法，所以不会
   * 撞掉任何在途意图。
   */
  const navigationRevision = useRef(0);
  /**
   * 一次**尚未完成的入场**（Creating）。
   *
   * 此时视图还没切过去，`active` 仍是 null 或主会话；但用户已经按了 `/back` 就
   * 意味着「别把我送进去」。没有这个记录，`back()` 会因为在看主会话而什么都不
   * 做，然后那个迟到的 open 回包把人拽进旁支（spec §5.2「创建尚未完成时，本地
   * `/back` 可撤销完成后进入的意图」）。
   */
  const entryIntent = useRef<{ hostKey: SideHostKey; parentId: string } | null>(null);
  /**
   * 本次停留（见 `SideEntry`）。`null` = 不在旁支里（包含「还没进入」）。
   *
   * 它是退出的**唯一**依据：票据、父、那一代、首问都从这里取。用 ref 而不是
   * state，因为退出必须在同一帧里同步失效——放进 state 要等下一次渲染，而
   * 迟到的回包可能在渲染之前就到了。
   */
  const entryRef = useRef<SideEntry | null>(null);
  const bumpNavigation = useCallback((): number => {
    navigationRevision.current += 1;
    return navigationRevision.current;
  }, []);
  /**
   * 每条变更锁按**完整来源身份**分开，而不是一个全局 busy。
   *
   * 一个 busy 会把「A 的开旁支还在路上」变成「B 也不能操作」；更糟的是它让本地
   * `/back` 变成「等网络」，而这正是 spec 明确禁止的。全局 `busy` 只留给显示。
   */
  const inFlight = useRef(new Set<string>());
  const lockKey = (hostKey: SideHostKey, parentId: string, action: string): string =>
    `${JSON.stringify([hostKey, parentId])}:${action}`;

  const setActiveBoth = useCallback((next: ActiveConversation | null) => {
    activeRef.current = next;
    setActive(next);
  }, []);

  const run = useCallback(async <T,>(fn: () => Promise<T>): Promise<T | null> => {
    setBusy(true);
    try {
      return await fn();
    } catch (e) {
      // 只暂存原因，**不**直接写成一条错误：这个通用包装不知道是哪条旁支失败
      // 的，冒然登记一条「来源未知」的错误会盖掉上一条有来源的、更准确的提示。
      // 调用方拿到 null 后再用 `recordError(message, target)` 补上归属。
      lastErrorRef.current = { message: formatError(e), target: null };
      return null;
    } finally {
      setBusy(false);
    }
  }, []);

  /**
   * 入场首问要在 `open` 里 `await`，而 `open` 的依赖列表必须稳定（它被 App 的
   * effect 闭包持有）。用转发 ref：调用时读最新实现，声明顺序不再形成循环。
   */
  const submitInternalRef = useRef<
    (
      target: SideTarget,
      text: string,
      binding: SideHostBinding,
      guard?: { onlyIfIntentCurrent: () => boolean; revision: number },
    ) => Promise<SideSubmitReceipt>
  >(async () => ({ kind: "rejected", requestId: "", message: "" }));

  /**
   * Observe 的「打开」= 找到已有的那条并查看。
   *
   * 三个约束都直接来自 spec §4.3：
   * - 只发 `side/list` / `side/readItems`（`reconcile` 恰好只用这两个），**零**
   *   `side/open`、零 `side/turnStart`。
   * - 带问题一律拒绝并说明：发问需要 Control，装成「打开了」只会让用户以为
   *   问题发出去了。
   * - 没有已存在的旁问时明确说「没有可查看的旁问」，不虚构一个空壳。
   */
  const observeOnly = useCallback(
    async (
      binding: SideHostBinding,
      parentId: string,
      question: string,
    ): Promise<boolean> => {
      if (question.trim() !== "") {
        recordError("只读设备只能查看旁问，不能发起提问", {
          kind: "side",
          hostKey: binding.key,
          parentId,
          sideId: "",
          generation: 0,
        });
        return false;
      }
      const existing = store
        .list(binding.key)
        .find((v) => v.parentId === parentId && !v.closed);
      const target: SideTarget = {
        kind: "side",
        hostKey: binding.key,
        parentId,
        sideId: existing?.sideId ?? "",
        generation: existing?.generation ?? 0,
      };
      setNotice(null);
      const key = lockKey(binding.key, parentId, "open");
      if (inFlight.current.has(key)) return true;
      inFlight.current.add(key);
      const revision = bumpNavigation();
      try {
        // 本地没有就用 list 发现：`side/list` 是 Observe 允许的读取，且它返回
        // 全部摘要，不会像 `side/open` 那样创建 runner。
        let sideId = existing?.sideId ?? "";
        if (sideId === "") {
          const sides = await run(async () => {
            const r = await binding.client.request<{ sides: SideSummaryWire[] }>(
              "side/list",
              { parentThreadId: parentId },
            );
            return r.sides;
          });
          if (sides === null) {
            recordError(lastErrorRef.current?.message ?? "读取旁问失败", target);
            return false;
          }
          const summary = sides.find((x) => x.parent_thread_id === parentId);
          if (summary === undefined) {
            recordNotice("这条会话还没有旁问");
            return false;
          }
          sideId = summary.side_id;
        }
        const view = store.read(binding.key, sideId);
        if (view === undefined) {
          // 发现了但本地还没有正文：`reconcile` 只做 list + readItems。
          await controllerFor(binding, parentId).reconcile(parentId);
          onChange();
        }
        if (navigationRevision.current !== revision) return true;
        const fresh = store.read(binding.key, sideId);
        if (fresh === undefined || fresh.closed) {
          recordNotice("这条会话还没有旁问");
          return false;
        }
        entryIntent.current = null;
        entryRef.current = {
          binding,
          parentId,
          target: null,
          summary: { sideId, generation: fresh.generation },
          question: null,
        };
        setActiveBoth({
          kind: "side",
          hostKey: binding.key,
          parentId,
          sideId,
          generation: fresh.generation,
        });
        return true;
      } finally {
        inFlight.current.delete(key);
      }
    },
    [
      store,
      run,
      controllerFor,
      onChange,
      setActiveBoth,
      bumpNavigation,
      recordError,
      recordNotice,
    ],
  );

  /**
   * 打开（或续用）某个父上的旁支。
   *
   * 返回值只表示「这条命令被接纳了」：首问的结果留在精确的 `SideView` 里（成功
   * 是 turnStarted，失败是 `rejectedSubmission`），不压成第二层 boolean。
   *
   * 入场是**异步之后**才发生的事，所以必须在发请求前抓下来源身份与导航修订：
   * 回包时用户可能已经返回、切到别的主会话，或者开了另一条旁支。
   */
  const open = useCallback(
    async (
      binding: SideHostBinding,
      parentId: string,
      question: string,
    ): Promise<boolean> => {
      if (!binding.supported) {
        recordError("当前宿主不支持旁问", null);
        return false;
      }
      if (!binding.snapshotLifecycle) {
        // 双能力位都要：只有旧位意味着宿主还是「返回后仍可恢复」的语义，
        // 而前端这块界面已经没有恢复入口了。
        recordError("当前宿主版本不支持入即可退出的旁问，请更新宿主", null);
        return false;
      }
      if (binding.scope === "observe") {
        // Observe 只能**查看**已有的旁问（spec §4.3）：`side/list` 与
        // `side/readItems` 是 Observe 级，`side/prepare` / `side/open` 不是。
        return observeOnly(binding, parentId, question);
      }
      // 上一代还没确认销毁：不能再建第二套（spec §6.1/§6.2「Closing 或清理未
      // 排空时显示上一条旁问正在退出，不并行建立第二条」）。
      if (exits.blocked(binding.instanceId, parentId)) {
        recordNotice("上一条旁问正在退出，请稍后重试");
        return false;
      }
      const key = lockKey(binding.key, parentId, "open");
      // 重复点击合并到同一次开场：服务端 `open` 幂等，但本地若并发走两遍
      // list/分页，会把同一条旁的正文对账两次。
      if (inFlight.current.has(key)) return true;
      inFlight.current.add(key);
      const revision = bumpNavigation();
      entryIntent.current = { hostKey: binding.key, parentId };
      const intentStillCurrent = () => navigationRevision.current === revision;
      const target: SideTarget = {
        kind: "side",
        hostKey: binding.key,
        parentId,
        sideId: "",
        generation: 0,
      };
      /** 仅在入场意图仍然有效时才真的换视图。 */
      const enterIfCurrent = (sideId: string, generation: number): boolean => {
        if (!intentStillCurrent()) return false;
        entryIntent.current = null;
        const next: SideTarget = { ...target, sideId, generation };
        setActiveBoth(next);
        return true;
      };

      // 本次停留从这里开始记录。**先建记录再发请求**：prepare/open 的迟到回包
      // 都要靠它判断「用户还在不在」，而记录一旦建立，退出就能拿到票据。
      const entry: SideEntry = {
        binding,
        parentId,
        target: null,
        summary: null,
        question: question.trim() === "" ? null : question,
      };
      entryRef.current = entry;

      try {
        // 1. 签票。参数校验（票据形状）与「是否属于这条宿主」在下面一起验。
        //
        // 已有未关闭的旁支（另一台设备开的，或本机开过但还没退出）也走这条路：
        // 服务端会把这张新票**绑定到同一代次**上（spec §6.2.2「多个端的不同
        // 票据可绑定同一 Creating/Open 代次，不刷新快照」）。所以这里不需要
        // 单独复用分支——而**必须**签票，否则本机退出时定位不到那一代，等于
        // 放弃了 spec §7「有 Control/Admin 权限的端可以销毁共享环境」。
        const prepared = await run(async () =>
          binding.client.request<{
            hostInstanceId: string;
            parentThreadId: string;
            entryId: string;
          }>("side/prepare", { parentThreadId: parentId }),
        );
        if (prepared === null) {
          entryRef.current = null;
          recordError(lastErrorRef.current?.message ?? "打开旁问失败", target);
          return false;
        }
        // 迟到的 prepare 回包描述的不是这次请求：撤销这张票（不留下环境），
        // 也不拿它去 open（spec §6.2.1「迟到回包只需撤销票据」）。
        if (
          prepared.hostInstanceId !== binding.instanceId ||
          prepared.parentThreadId !== parentId ||
          typeof prepared.entryId !== "string" ||
          prepared.entryId === ""
        ) {
          entryRef.current = null;
          recordError("宿主返回了不匹配的入场票据", target);
          return false;
        }
        entry.target = {
          hostInstanceId: prepared.hostInstanceId,
          parentThreadId: prepared.parentThreadId,
          entryId: prepared.entryId,
        };
        // 拿到票之后用户已经离开：只撤票，不 open、不建环境。
        if (!intentStillCurrent() || entryRef.current !== entry) {
          const t = entry.target;
          entry.target = null;
          entryRef.current = null;
          // 上面已经排除了 `observe`：走到这里一定有 Control。
          if (t !== null) {
            exits.record(t);
            void exits.flush(binding);
          }
          return true;
        }

        // 2. 用票 open。
        const summary = await run(async () =>
          binding.client.request<SideSummaryWire>("side/open", {
            parentThreadId: parentId,
            entryId: prepared.entryId,
          }),
        );
        if (summary === null) {
          // open 失败：票还留着，它指向一代并未建立的环境。记成退出意图让宿主
          // 回收（幂等），同时撤掉本地记录。
          const t = entry.target;
          entry.target = null;
          entryRef.current = null;
          if (t !== null) {
            exits.record(t);
            void exits.flush(binding);
          }
          recordError(lastErrorRef.current?.message ?? "打开旁问失败", target);
          return false;
        }
        entry.summary = { sideId: summary.side_id, generation: summary.generation };

        // 用户在这次 open 在途时已经离开了（spec §6.2.5「open 的迟到回包必须
        // 使本代销毁」）。此时**不要**对账——那会把一批即将被销毁的帧拉回本地
        // 视图；直接把刚建立的这一代连同票据一起送去销毁，并且不提交首问、
        // 不存草稿。
        if (!intentStillCurrent() || entryRef.current !== entry) {
          const t = entry.target;
          entry.target = null;
          entry.question = null;
          entryRef.current = null;
          const late = controllerFor(binding, parentId);
          late.invalidateGeneration(summary.side_id, summary.generation);
          store.dropGeneration(binding.key, summary.side_id, summary.generation);
          if (t !== null) {
            exits.record(t);
            void exits.flush(binding);
          }
          onChange();
          return true;
        }

        // 这次的 open 已经在服务端生效：把它对账进原身份，否则这条旁支在本地
        // 永远缺一份正文。
        const controller = controllerFor(binding, parentId);
        await controller.reconcile(parentId);
        onChange();
        enterIfCurrent(summary.side_id, summary.generation);
        if (entry.question !== null) {
          // `reconcile` 期间用户可能离开：同样不替他发出这句还没确认的问题。
          if (!intentStillCurrent() || entryRef.current !== entry) return true;
          await submitInternalRef.current(
            {
              ...target,
              sideId: summary.side_id,
              generation: summary.generation,
            },
            entry.question,
            binding,
            { onlyIfIntentCurrent: intentStillCurrent, revision },
          );
        }
        return true;
      } finally {
        inFlight.current.delete(key);
      }
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [
      store,
      controllerFor,
      onChange,
      run,
      setActiveBoth,
      bumpNavigation,
      observeOnly,
      recordError,
      recordNotice,
      exits,
    ],
  );

  /**
   * 提交到**指定的**旁支身份。
   *
   * 参数里没有「当前」：目标、binding 都在出发时定格，`await` 之后 store 可能
   * 已经换了一代，或者用户切到了另一个宿主上的同名 sideId。控制器绑定的是
   * `(hostKey, parentId)`，所以这次提交永远落在它出发时那条旁支上。
   */
  const submitInternal = useCallback(
    async (
      target: SideTarget,
      text: string,
      binding: SideHostBinding,
      guard?: { onlyIfIntentCurrent: () => boolean; revision: number },
    ): Promise<SideSubmitReceipt> => {
      const current = store.read(target.hostKey, target.sideId);
      if (!current || current.parentId !== target.parentId) {
        return {
          kind: "rejected",
          requestId: "",
          message: "这条旁问已经不存在了",
        };
      }
      if (current.generation !== target.generation) {
        return {
          kind: "rejected",
          requestId: "",
          message: "这条旁问已被新的会话取代",
        };
      }
      if (guard && !guard.onlyIfIntentCurrent()) {
        // 入场意图已撤销：不替用户发出他还没确认的问题。
        return {
          kind: "rejected",
          requestId: "",
          message: "已返回主会话，未发送这条消息",
        };
      }
      const key = lockKey(target.hostKey, target.parentId, "submit");
      if (inFlight.current.has(key)) {
        return {
          kind: "rejected",
          requestId: "",
          message: "上一条消息还在发送中",
        };
      }
      inFlight.current.add(key);
      const requestId = newRequestId();
      try {
        const controller = controllerFor(binding, target.parentId);
        const receipt = await controller.submit(target.sideId, text, requestId);
        if (receipt.kind === "rejected" && receipt.message !== "") {
          // 失败原因按**来源**保存：写当前会话的错误横幅会把 A 的失败说成 B 的。
          recordError(receipt.message, target);
        } else if (receipt.kind === "accepted") {
          clearScopedError(target);
        }
        return receipt;
      } catch (e) {
        const message = formatError(e);
        recordError(message, target);
        return { kind: "rejected", requestId, message };
      } finally {
        inFlight.current.delete(key);
        onChange();
      }
    },
    [store, controllerFor, onChange, newRequestId, recordError, clearScopedError],
  );
  submitInternalRef.current = submitInternal;

  const submit = useCallback(
    async (text: string): Promise<SideSubmitReceipt> => {
      const current = activeRef.current;
      if (current === null || current.kind !== "side") {
        return { kind: "rejected", requestId: "", message: "当前没有打开的旁问" };
      }
      const binding = store.binding(current.hostKey);
      if (!binding) {
        return { kind: "rejected", requestId: "", message: "旁问所在的项目没有连接" };
      }
      return submitInternal(current, text, binding);
    },
    [store, submitInternal],
  );

  const enter = useCallback(
    (
      binding: SideHostBinding,
      parentId: string,
      sideId: string,
      generation: number,
    ) => {
      bumpNavigation();
      entryIntent.current = null;
      // 进入一条已经存在的旁问不算「本次停留」：本机没有它的票据，退出也就
      // 无从定位（见 `open` 里复用分支的注释）。记录仍建立，这样退出至少能丢
      // 掉本地正文并停掉在途的对账。
      entryRef.current = {
        binding,
        parentId,
        target: null,
        summary: { sideId, generation },
        question: null,
      };
      setActiveBoth({ kind: "side", hostKey: binding.key, parentId, sideId, generation });
    },
    [setActiveBoth, bumpNavigation],
  );

  /**
   * 结束本次停留的**本地**部分。
   *
   * 顺序是刻意的，每一步都对应一条具体的错误：
   *
   * 1. 先清「停留」记录。任何在途回包（prepare/open/分页）此后再检查它都会
   *    看到 null，于是不会把用户送回一个已经离开的视图。**必须同步**——放在
   *    一个 `await` 之后就已经晚了。
   * 2. 再记退出意图。票据在退出这一刻从记录里取（不是从「当前选中的会话」推
   *    断）。记录之后，`blocked()` 立刻对这一父成立，本地不能再建第二套。
   * 3. 只读端（`observe`）不记：它没有关闭共享环境的权限，退出只清本地视图。
   *    这一点由 `SideExitCoordinator` 再判一次（那里还有「宿主不支持」等）。
   * 4. 丢掉这一代正文、草稿、用量与分页水位。**不做 `reconcile`**——退出不是
   *    「重新对齐」，再去问服务端反而会把一批即将被销毁的帧拉回来。
   * 5. 最后补发一次退出。不 await：导航不能等网络。
   */
  const endEntry = useCallback(
    (entry: SideEntry | null, reason: LeaveReason): void => {
      if (entry === null) return;
      const { binding, parentId, target, summary } = entry;
      // 首问与摘要都属于这次停留，退出即刻丢弃（spec §4.3/§4.4）。
      entry.question = null;
      if (target !== null) {
        // `flag`：本机实验开关被关掉。宿主自己会在后台 `close_all` 把本机环境
        // 全部收掉，此时再发 `side/exit` 只会得到 `-32601`，并且会在账本里留下
        // 一条永远确认不了的记录——那会把这条父永久锁在「正在退出」上。所以这里
        // 只丢本地视图与正文，退出交给宿主。
        if (binding.scope !== "observe" && reason !== "flag") exits.record(target);
        if (summary !== null) {
          const controller = controllerFor(binding, parentId);
          controller.invalidateGeneration(summary.sideId, summary.generation);
          store.dropGeneration(binding.key, summary.sideId, summary.generation);
        }
        if (reason !== "flag") void exits.flush(binding);
      } else if (summary !== null) {
        // 没有票据（例如旧宿主）时至少把正文丢掉，不留一份可被「恢复」的视图。
        const controller = controllerFor(binding, parentId);
        controller.invalidateGeneration(summary.sideId, summary.generation);
        store.dropGeneration(binding.key, summary.sideId, summary.generation);
      }
      onChange();
    },
    [exits, store, controllerFor, onChange],
  );

  /**
   * 唯一的退出口（spec §4.3）。
   *
   * 所有「从旁支离开」的动作都走它：顶栏返回、`/back`、切到另一条普通会话、
   * `/side-close`、项目/宿主切换、应用卸载、本机开关被关。
   */
  const leaveSide = useCallback(
    (reason: LeaveReason) => {
      const entry = entryRef.current;
      // 先加修订号再清记录：迟到的 open 回包必须立刻看不见任何授权。
      bumpNavigation();
      entryRef.current = null;
      entryIntent.current = null;
      const current = activeRef.current;
      endEntry(entry, reason);
      if (reason === "navigation") {
        // 视图由导航自己接管（它会立刻 `selectMain` 到新的那条），这里不抢。
        return;
      }
      if (current !== null && current.kind === "main") return;
      const from = current ?? entry;
      if (from === null) return;
      const hostKey = "binding" in from ? from.binding.key : from.hostKey;
      const parentId = from.parentId;
      setActiveBoth({ kind: "main", hostKey, parentId });
    },
    [bumpNavigation, endEntry, setActiveBoth],
  );

  const back = useCallback(() => leaveSide("back"), [leaveSide]);

  const selectMain = useCallback(
    (binding: SideHostBinding, parentId: string) => {
      const current = activeRef.current;
      // 从旁支切到另一条普通会话也是退出（spec §4.3）。已经在主视图里就不算
      // 离开，只是换当前会话。
      if (current !== null && current.kind === "side") {
        leaveSide("navigation");
      } else {
        bumpNavigation();
        entryIntent.current = null;
      }
      // 已经在主视图里也必须把 `parentId` 换成刚选中的那条：主视图的这个字段
      // 就是「当前会话」的身份，输入框的会话键由它派生。提前返回会让切会话后
      // 身份停在旧值——两条会话于是共用一个键，一次在途提交/长请求就能锁住
      // 另一条的 Send。
      if (current !== null && current.kind === "main" && current.parentId === parentId) return;
      setActiveBoth({ kind: "main", hostKey: binding.key, parentId });
    },
    [setActiveBoth, bumpNavigation, leaveSide],
  );

  const stop = useCallback(async () => {
    const current = activeRef.current;
    if (current === null || current.kind !== "side") return;
    const view = store.read(current.hostKey, current.sideId);
    if (!view) return;
    const turnId = view.summary.active_turn_id;
    if (turnId === null) return;
    const binding = store.binding(current.hostKey);
    if (!binding) return;
    // 目标、client、轮次在这里就固定下来：`side/turnInterrupt` 必须打给**这一轮**，
    // 不能打给之后新起的一轮，也不能因为用户切到另一个宿主上的同名 sideId 而
    // 打到别人身上。
    const target: SideTarget = { ...current };
    const client = binding.client;
    const key = lockKey(target.hostKey, target.parentId, "stop");
    if (inFlight.current.has(key)) return;
    inFlight.current.add(key);
    try {
      const ok = await run(async () => {
        await client.request("side/turnInterrupt", {
          sideId: target.sideId,
          turnId,
        });
        return true;
      });
      if (ok === null) {
        recordError(lastErrorRef.current?.message ?? "停止旁问失败", target);
        return;
      }
    } finally {
      inFlight.current.delete(key);
      onChange();
    }
  }, [store, run, onChange, recordError]);

  /**
   * 在主视图里关掉**后台**那条旁支（`/side-close`）。
   *
   * 与 `leaveSide` 的区别只有出发位置：这里不在旁支视图里，所以父身份要由调用
   * 方给。行为完全相同——记退出意图、丢本地正文、补发一次退出。
   */
  const closeBackground = useCallback(
    async (binding: SideHostBinding, parentId: string): Promise<void> => {
      if (!binding.supported) {
        recordError("当前宿主不支持旁问", null);
        return;
      }
      if (binding.scope === "observe") {
        recordError("只读设备不能关闭旁问", null);
        return;
      }
      setNotice(null);
      const key = lockKey(binding.key, parentId, "close");
      if (inFlight.current.has(key)) return;
      inFlight.current.add(key);
      try {
        const entry = entryRef.current;
        if (entry !== null && entry.binding.key === binding.key && entry.parentId === parentId) {
          // 本次停留就是它：走同一条退出路径，票据已经在手。
          leaveSide("close");
          return;
        }
        // 后台旁支（本次停留不是它，或根本不是本会话开的）：本地视图里的那一代
        // 可以作为退出的起点，但**不**凭它猜一个票据——票据只在真正的入场时
        // 签发过。没有票据时如实说明，不虚构一次关闭。
        //
        // 先问一次服务端：本地视图为空**不等于**没有旁支（可能是另一端刚开的，
        // 也可能本机从未看到过它）。直接说「没有打开的旁问」会让用户以为已经
        // 清干净，而宿主上还占着唯一性槽位。`side/list` 是只读，代价可忽略。
        let view = store
          .list(binding.key)
          .find((v) => v.parentId === parentId && !v.closed);
        if (view === undefined) {
          const listed = await run(async () =>
            binding.client.request<{ sides?: unknown }>("side/list", {
              parentThreadId: parentId,
            }),
          );
          const sides = Array.isArray(listed?.sides) ? listed.sides : [];
          const alive = sides.filter(
            (row): row is { side_id: string } =>
              typeof row === "object" &&
              row !== null &&
              typeof (row as { side_id?: unknown }).side_id === "string" &&
              (row as { side_id: string }).side_id !== "",
          );
          if (alive.length === 0) {
            recordNotice("当前没有打开的旁问");
            return;
          }
          // 有，但不是本机开的：定位不到那一代，如实说明。
          recordError(
            "这条旁问不是本设备打开的，无法从本机退出；请在打开它的设备上返回",
            {
              kind: "side",
              hostKey: binding.key,
              parentId,
              sideId: alive[0].side_id,
              generation: 0,
            },
          );
          return;
        }
        recordError(
          "这条旁问不是本设备打开的，无法从本机退出；请在打开它的设备上返回",
          {
            kind: "side",
            hostKey: binding.key,
            parentId,
            sideId: view.sideId,
            generation: view.generation,
          },
        );
      } finally {
        inFlight.current.delete(key);
        onChange();
      }
    },
    [store, leaveSide, onChange, recordError, recordNotice, run],
  );

  const clearError = useCallback(() => {
    setError(null);
    setErrorTarget(null);
    setNotice(null);
  }, []);

  const visibleError = (() => {
    const a = active;
    if (error === null || errorTarget === null) return null;
    if (a === null || a.kind !== "side") return null;
    return sameTarget(a, errorTarget) ? error : null;
  })();

  return {
    active,
    open,
    back,
    selectMain,
    enter,
    submit,
    stop,
    leaveSide,
    closeBackground,
    busy,
    error,
    errorTarget,
    visibleError,
    notice,
    clearError,
  };
}

/** 供外部判断一条旁支能否下发 Control 操作。 */
export function canControlSide(binding: SideHostBinding | undefined): boolean {
  return binding !== undefined && binding.supported && binding.scope !== "observe";
}

/** 旁支顶栏要显示的父状态文案。 */
export function parentStatusText(
  status: string | null | undefined,
): string {
  switch (status) {
    case "running":
      return "运行中";
    case "awaiting_approval":
      return "等待审批";
    case "idle":
      return "空闲";
    case "failed":
      return "失败";
    default:
      return status ?? "未知";
  }
}

/** 供测试与调用方复用的客户端类型别名。 */
export type SideClient = RpcClient;
