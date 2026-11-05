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
  /** 取（必要时创建）某宿主上某个父的对账控制器。 */
  controllerFor: (binding: SideHostBinding, parentId: string) => SideSyncController;
  /** store 变化后的重渲染。 */
  onChange: () => void;
  /** 生成去重用的规范 UUID（测试可注入，避免断言依赖随机值）。 */
  newRequestId?: () => string;
}

/**
 * 关闭前的确认。
 *
 * 这里带的是**完整的出发身份**，不是「当前旁支」：确认弹窗开着的时候用户可能
 * 已经切走、切回，或者连接换了。确认一旦生成，就只对这份身份有效——
 * `target` + `client` 决定请求打给谁，`navigationRevision` 决定成功后**要不要**
 * 把用户送回父会话（中途走开的人不该被一个过期的确认拽回去）。
 */
export interface CloseConfirmation {
  target: SideTarget;
  client: RpcClient;
  /** 生成这份确认时的导航修订；成功后必须仍然相同才回父。 */
  navigationRevision: number;
  /** `null` = 服务端说它空闲；非 null = 正在跑的那一轮，需要用户确认后传回。 */
  activeTurnId: string | null;
  running: boolean;
}

export interface SideConversationController {
  active: ActiveConversation | null;
  /** 打开（或续用）某个父上的旁支。返回是否成功受理。 */
  open: (binding: SideHostBinding, parentId: string, question: string) => Promise<boolean>;
  /** 把当前视图切回主会话。**不**停旁支、不关闭；也能撤销一个尚未完成的入场。 */
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
   * 请求关闭：先读这个父在服务端的**当前**摘要，把确认所需信息交回 UI。
   *
   * 传 `binding`/`parentId` 而不是「当前旁支」：主视图也要能关闭后台旁问。
   * 返回 `null` 表示没有可关闭的对象（已关闭、或这个父本来就没有旁问），
   * 此时只留下一条幂等提示，绝不虚构一次关闭。
   */
  requestClose: (
    binding: SideHostBinding,
    parentId: string,
  ) => Promise<CloseConfirmation | null>;
  /** 用户确认后真正关闭。`expectedActiveTurnId` 必须来自 `requestClose`。 */
  confirmClose: (confirmation: CloseConfirmation) => Promise<boolean>;
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
  const { store, controllerFor, onChange } = deps;
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
      if (binding.scope === "observe") {
        // Observe 只能**查看**已有的旁问（spec §4.3）：`side/list` 与
        // `side/readItems` 是 Observe 级，`side/open` 不是。所以这里绝不发
        // open，而是把服务端已有的那条找出来、装进本地再导航过去。
        return observeOnly(binding, parentId, question);
      }
      const key = lockKey(binding.key, parentId, "open");
      // 重复点击合并到同一次开场：服务端 `side/open` 幂等，但本地若并发走两遍
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

      try {
        // 已有未关闭的旁支：直接续用，不再请求服务端。
        const existing = store
          .list(binding.key)
          .find((v) => v.parentId === parentId && !v.closed);
        if (existing) {
          enterIfCurrent(existing.sideId, existing.generation);
          if (question.trim() === "") return true;
          // 重新对账一次：切回来时正文可能是旧的。首问照样发，但只在这个意图
          // 还有效时——用户已经返回的话，问题留在原草稿里（Task 2 的 rejected
          // 记录同样落在那条旁支上）。
          const controller = controllerFor(binding, parentId);
          await controller.reconcile(parentId);
          onChange();
          await submitInternalRef.current(
            { ...target, sideId: existing.sideId, generation: existing.generation },
            question,
            binding,
            { onlyIfIntentCurrent: intentStillCurrent, revision },
          );
          return true;
        }

        const summary = await run(async () =>
          binding.client.request<SideSummaryWire>("side/open", {
            parentThreadId: parentId,
          }),
        );
        if (summary === null) {
          recordError(lastErrorRef.current?.message ?? "打开旁问失败", target);
          return false;
        }

        // 这次的 open 已经在服务端生效：无论用户是否还在这里，都要把它对账进
        // 原身份（否则这条旁支在本地永远缺一份正文）。
        const controller = controllerFor(binding, parentId);
        await controller.reconcile(parentId);
        onChange();
        enterIfCurrent(summary.side_id, summary.generation);
        if (question.trim() !== "") {
          // 首问的「是否还该发」与「入场」是两件事：用户返回之后不能再替他在
          // 后台发出一个他还没确认的问题（spec §5.2）。
          const receipt = await submitInternalRef.current(
            {
              ...target,
              sideId: summary.side_id,
              generation: summary.generation,
            },
            question,
            binding,
            { onlyIfIntentCurrent: intentStillCurrent, revision },
          );
          if (!intentStillCurrent() && receipt.kind === "rejected") {
            // 用户已经返回：不替他发出这句还没确认的问题，但也**不能丢**——
            // 留成那条旁问的草稿，他之后再 `/side` 进来还能看见并发送
            // （spec §5.2）。已有草稿时不覆盖（那是他自己正在写的东西）。
            const view = store.read(target.hostKey, summary.side_id);
            if (view && view.draft.trim() === "") {
              store.setDraft(target.hostKey, summary.side_id, question);
            }
          }
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
      setActiveBoth({ kind: "side", hostKey: binding.key, parentId, sideId, generation });
    },
    [setActiveBoth, bumpNavigation],
  );

  const back = useCallback(() => {
    const current = activeRef.current;
    // 连一个还没完成的 open 也一并撤销：本地返回绝不能等网络，也不能被一个迟到
    // 的回包推翻。先加修订号，再决定要不要切换视图。
    bumpNavigation();
    const pending = entryIntent.current;
    entryIntent.current = null;
    // 返回只是换个指针。停旁支要显式点 Stop，关闭要显式确认。
    const from = current ?? pending;
    if (from === null) return;
    if (current !== null && current.kind === "main") return;
    setActiveBoth({ kind: "main", hostKey: from.hostKey, parentId: from.parentId });
  }, [setActiveBoth, bumpNavigation]);

  const selectMain = useCallback(
    (binding: SideHostBinding, parentId: string) => {
      const current = activeRef.current;
      bumpNavigation();
      entryIntent.current = null;
      if (current !== null && current.kind === "main") return;
      setActiveBoth({ kind: "main", hostKey: binding.key, parentId });
    },
    [setActiveBoth, bumpNavigation],
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
   * 关闭的第一步：读**这个父**在服务端的当前摘要，把确认所需信息交回 UI。
   *
   * 为什么必须读服务端而不是本地缓存：本地可能还停在 idle，而旁支其实已经跑
   * 起来了。关掉一个正在回答的旁支与关掉一个空闲旁支是两种确认，不能猜。
   *
   * 为什么用 `side/list` 而不是给单条旁支加读接口：`list` 已经是 Observe 级、
   * 返回全部摘要、且**不产生新状态**。找不到那条旁支意味着它已被另一端关掉，
   * 此时只留一条幂等提示，绝不虚构一次关闭。
   */
  /**
   * 若当前正显示这个父的某条旁支，就切回主会话。
   *
   * 传 `guard` 时还要求「显示的就是那条」且 `navigationRevision` 仍是生成确认时
   * 的那一个——关闭是在等网络，期间用户可能已经返回、进了别的旁支，或者重新
   * 导航过。这种情况下切换视图是替他做决定。
   */
  const leaveIfShowing = useCallback(
    (
      hostKey: SideHostKey,
      parentId: string,
      guard: { sideId: string; revision: number } | null,
    ): void => {
      const current = activeRef.current;
      if (current === null || current.kind !== "side") return;
      if (current.hostKey !== hostKey || current.parentId !== parentId) return;
      if (guard !== null) {
        if (current.sideId !== guard.sideId) return;
        if (navigationRevision.current !== guard.revision) return;
      }
      setActiveBoth({ kind: "main", hostKey, parentId });
    },
    [setActiveBoth],
  );

  const requestClose = useCallback(
    async (
      binding: SideHostBinding,
      parentId: string,
    ): Promise<CloseConfirmation | null> => {
      if (!binding.supported) {
        recordError("当前宿主不支持旁问", null);
        return null;
      }
      if (binding.scope === "observe") {
        recordError("只读设备不能关闭旁问", null);
        return null;
      }
      setNotice(null);
      const key = lockKey(binding.key, parentId, "close");
      if (inFlight.current.has(key)) return null;
      inFlight.current.add(key);
      // 目标身份在**出发前**定格：确认弹窗开着的时候用户可能已经切走。
      const revision = bumpNavigation();
      const target: SideTarget = {
        kind: "side",
        hostKey: binding.key,
        parentId,
        sideId: "",
        generation: 0,
      };
      try {
        const sides = await run(async () => {
          const r = await binding.client.request<{ sides: SideSummaryWire[] }>(
            "side/list",
            { parentThreadId: parentId },
          );
          return r.sides;
        });
        if (sides === null) {
          recordError(lastErrorRef.current?.message ?? "读取旁问失败", target);
          return null;
        }
        const summary = sides.find(
          (x) => x.parent_thread_id === parentId && x.side_id !== "",
        );
        if (summary === undefined) {
          recordNotice("当前没有打开的旁问");
          leaveIfShowing(binding.key, parentId, null);
          return null;
        }
        return {
          target: {
            ...target,
            sideId: summary.side_id,
            generation: summary.generation,
          },
          client: binding.client,
          navigationRevision: revision,
          activeTurnId: summary.active_turn_id,
          running: summary.status === "running",
        };
      } finally {
        inFlight.current.delete(key);
        onChange();
      }
    },
    [run, onChange, bumpNavigation, recordError, recordNotice],
  );

  /**
   * 用户确认后真正关闭。
   *
   * 发请求前把 `confirmation` 与**当前**绑定再核对一次：连接换了（client 变了）
   * 就不能往旧 client 上发；权限降成 Observe 也不能发。请求用的是
   * `confirmation.client` 与 `confirmation.activeTurnId`——即用户确认时看到的那
   * 一轮，而不是「此刻 store 里最新的那一轮」。新起的一轮绝不能被这次关闭顺手
   * 停掉（服务端会按 expectedActiveTurnId 拒绝，这正是它的作用）。
   */
  const confirmClose = useCallback(
    async (confirmation: CloseConfirmation): Promise<boolean> => {
      const { target } = confirmation;
      const binding = store.binding(target.hostKey);
      if (!binding) {
        recordError("旁问所在的项目没有连接", target);
        return false;
      }
      if (binding.client !== confirmation.client) {
        recordError("连接已经更换，请重新确认关闭", target);
        return false;
      }
      if (!binding.supported || binding.scope === "observe") {
        recordError("当前权限不能关闭旁问", target);
        return false;
      }
      // 换代：这份确认描述的那一代已经不在了，关它没有意义（也不安全）。
      const view = store.read(target.hostKey, target.sideId);
      if (view !== undefined && view.generation !== target.generation) {
        recordError("这条旁问已经被新的会话取代", target);
        return false;
      }
      setNotice(null);
      const key = lockKey(target.hostKey, target.parentId, "close");
      if (inFlight.current.has(key)) return false;
      inFlight.current.add(key);
      try {
        const ok = await run(async () => {
          await confirmation.client.request("side/close", {
            sideId: target.sideId,
            expectedActiveTurnId: confirmation.activeTurnId,
          });
          return true;
        });
        if (ok !== true) {
          recordError(lastErrorRef.current?.message ?? "关闭旁问失败", target);
          return false;
        }
        // 只有「还看着这一条」且「用户没再导航过」才把视图送回父会话。在等待
        // 期间走开又回来的人，不该被一份过期确认强行送走。
        leaveIfShowing(
          target.hostKey,
          target.parentId,
          { sideId: target.sideId, revision: confirmation.navigationRevision },
        );
        await controllerFor(binding, target.parentId).reconcile(target.parentId);
        onChange();
        return true;
      } finally {
        inFlight.current.delete(key);
        onChange();
      }
    },
    [store, controllerFor, run, onChange, recordError],
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
    requestClose,
    confirmClose,
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
