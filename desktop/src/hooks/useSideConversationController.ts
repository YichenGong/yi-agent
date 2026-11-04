import { useCallback, useRef, useState } from "react";
import type { RpcClient } from "../lib/rpc";
import { formatError } from "../lib/errorMessage";
import type { SideSummaryWire } from "../lib/generated/SideSummaryWire";
import type {
  SideConversationStore,
  SideHostBinding,
  SideHostKey,
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

/** 关闭前的确认需要服务端**当前**的 turn 身份，不能拿本地缓存去猜。 */
export interface CloseConfirmation {
  sideId: string;
  /** `null` = 服务端说它空闲；非 null = 正在跑的那一轮，需要用户确认后传回。 */
  activeTurnId: string | null;
  running: boolean;
}

export interface SideConversationController {
  active: ActiveConversation | null;
  /** 打开（或续用）某个父上的旁支。返回是否成功受理。 */
  open: (binding: SideHostBinding, parentId: string, question: string) => Promise<boolean>;
  /** 把当前视图切回主会话。**不**停旁支、不关闭。 */
  back: () => void;
  /** 进入某个已存在的旁支（Observe 也能用：list 里看到的条目）。 */
  enter: (
    binding: SideHostBinding,
    parentId: string,
    sideId: string,
    generation: number,
  ) => void;
  submit: (text: string) => Promise<boolean>;
  stop: () => Promise<void>;
  /** 请求关闭：先读当前 summary，把确认所需信息交回 UI。 */
  requestClose: () => Promise<CloseConfirmation | null>;
  /** 用户确认后真正关闭。`expectedActiveTurnId` 必须来自 `requestClose`。 */
  confirmClose: (confirmation: CloseConfirmation) => Promise<boolean>;
  /** 正在处理一条旁支 RPC（按钮据此禁用）。 */
  busy: boolean;
  /** 最近一次旁支操作的失败原因。 */
  error: string | null;
  /** 清掉错误（用户已看到）。 */
  clearError: () => void;
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
  // 异步动作在 await 之后需要读到"最新"的 active，但读取必须显式，不能让它
  // 在闭包里悄悄变旧。这里同时保留一份 ref 影子。
  const activeRef = useRef<ActiveConversation | null>(null);
  activeRef.current = active;

  const setActiveBoth = useCallback((next: ActiveConversation | null) => {
    activeRef.current = next;
    setActive(next);
  }, []);

  const run = useCallback(async <T,>(fn: () => Promise<T>): Promise<T | null> => {
    setBusy(true);
    setError(null);
    try {
      return await fn();
    } catch (e) {
      setError(formatError(e));
      return null;
    } finally {
      setBusy(false);
    }
  }, []);

  const open = useCallback(
    async (
      binding: SideHostBinding,
      parentId: string,
      question: string,
    ): Promise<boolean> => {
      if (!binding.supported) {
        setError("当前宿主不支持旁问");
        return false;
      }
      // 重复点击必须合并到同一个旁支：服务端的 `side/open` 幂等，但本地若每次都
      // 走一遍 list/分页，会把同一条旁支对账两次并丢掉第一条问题。
      const existing = store
        .list(binding.key)
        .find((v) => v.parentId === parentId && !v.closed);
      if (existing) {
        setActiveBoth({
          kind: "side",
          hostKey: binding.key,
          parentId,
          sideId: existing.sideId,
          generation: existing.generation,
        });
        if (question.trim() !== "") return (await submitInternal(existing.sideId, question));
        return true;
      }

      const result = await run(async () => {
        const summary = await binding.client.request<SideSummaryWire>("side/open", {
          parentThreadId: parentId,
        });
        return summary;
      });
      if (result === null) return false;

      setActiveBoth({
        kind: "side",
        hostKey: binding.key,
        parentId,
        sideId: result.side_id,
        generation: result.generation,
      });
      const controller = controllerFor(binding, parentId);
      await controller.reconcile(parentId);
      onChange();
      if (question.trim() !== "") {
        return await submitInternal(result.side_id, question);
      }
      return true;
      // eslint-disable-next-line react-hooks/exhaustive-deps
    },
    [store, controllerFor, onChange, run, setActiveBoth],
  );

  /**
   * 提交到某条旁支。
   *
   * 捕获 `sideId` 与控制器：`await` 之后不再读 `activeRef`。控制器绑定的是
   * `(hostKey, parentId)`，所以即使此刻 UI 已经切回父会话，这次提交仍然落在
   * 它出发时的那条旁支上。
   */
  const submitInternal = useCallback(
    async (sideId: string, text: string): Promise<boolean> => {
      const found = store.findView(sideId);
      if (!found) {
        setError("这条旁问已经不存在了");
        return false;
      }
      const binding = store.binding(found.hostKey);
      if (!binding) {
        setError("旁问所在的项目没有连接");
        return false;
      }
      const controller = controllerFor(binding, found.view.parentId);
      const receipt = await run(async () => controller.submit(sideId, text, newRequestId()));
      onChange();
      // 只有明确受理才算「命令已接纳」；不确定与拒绝都要让调用方按各自语义处理。
      return receipt?.kind === "accepted";
    },
    [store, controllerFor, onChange, run, newRequestId],
  );

  const submit = useCallback(
    async (text: string): Promise<boolean> => {
      const current = activeRef.current;
      if (current === null || current.kind !== "side") return false;
      return submitInternal(current.sideId, text);
    },
    [submitInternal],
  );

  const enter = useCallback(
    (
      binding: SideHostBinding,
      parentId: string,
      sideId: string,
      generation: number,
    ) => {
      setActiveBoth({ kind: "side", hostKey: binding.key, parentId, sideId, generation });
    },
    [setActiveBoth],
  );

  const back = useCallback(() => {
    const current = activeRef.current;
    // 返回只是换个指针。停旁支要显式点 Stop，关闭要显式确认。
    if (current === null || current.kind === "main") return;
    setActiveBoth({ kind: "main", hostKey: current.hostKey, parentId: current.parentId });
  }, [setActiveBoth]);

  const stop = useCallback(async () => {
    const current = activeRef.current;
    if (current === null || current.kind !== "side") return;
    const view = store.read(current.hostKey, current.sideId);
    if (!view) return;
    const turnId = view.summary.active_turn_id;
    if (turnId === null) return;
    const binding = store.binding(current.hostKey);
    if (!binding) return;
    // 目标在这里就固定下来：`side/turnInterrupt` 必须打给这一轮，不能打给
    // 之后新起的一轮。
    await run(async () => {
      await binding.client.request("side/turnInterrupt", {
        sideId: current.sideId,
        turnId,
      });
      return true;
    });
    onChange();
  }, [store, run, onChange]);

  const requestClose = useCallback(async (): Promise<CloseConfirmation | null> => {
    const current = activeRef.current;
    if (current === null || current.kind !== "side") return null;
    const binding = store.binding(current.hostKey);
    if (!binding) {
      setError("旁问所在的项目没有连接");
      return null;
    }
    // 必须向服务端要一份**当前**的状态：本地缓存可能还停在 idle，而旁支其实
    // 已经跑起来了。关掉一个正在回答的旁支与关掉一个空闲旁支是两种确认。
    //
    // 用 `side/list` 而不是给单条旁支加读接口：`list` 已经是 Observe 级、返回
    // 全部摘要，且**不产生新状态**。这里找不到那条旁支意味着它已经被另一端
    // 关掉了，回主会话即可，不能拿本地缓存的旧 turn 去关。
    const sides = await run(async () => {
      const r = await binding.client.request<{ sides: SideSummaryWire[] }>(
        "side/list",
        { parentThreadId: current.parentId },
      );
      return r.sides;
    });
    if (sides === null) return null;
    const summary = sides.find((s) => s.side_id === current.sideId);
    if (summary === undefined) {
      setActiveBoth({
        kind: "main",
        hostKey: current.hostKey,
        parentId: current.parentId,
      });
      return null;
    }
    return {
      sideId: current.sideId,
      activeTurnId: summary.active_turn_id,
      running: summary.status === "running",
    };
  }, [store, run]);

  const confirmClose = useCallback(
    async (confirmation: CloseConfirmation): Promise<boolean> => {
      const current = activeRef.current;
      const found = store.findView(confirmation.sideId);
      const hostKey = current?.hostKey ?? found?.hostKey;
      if (hostKey === undefined) return false;
      const binding = store.binding(hostKey);
      if (!binding) return false;
      const parentId = current?.parentId ?? found?.view.parentId ?? null;
      const ok = await run(async () => {
        await binding.client.request("side/close", {
          sideId: confirmation.sideId,
          expectedActiveTurnId: confirmation.activeTurnId,
        });
        return true;
      });
      if (ok !== true) return false;
      if (parentId !== null) {
        setActiveBoth({ kind: "main", hostKey, parentId });
      }
      const controller = controllerFor(binding, parentId ?? "");
      if (parentId !== null) {
        await controller.reconcile(parentId);
      }
      onChange();
      return true;
    },
    [store, controllerFor, run, onChange, setActiveBoth],
  );

  const clearError = useCallback(() => setError(null), []);

  return {
    active,
    open,
    back,
    enter,
    submit,
    stop,
    requestClose,
    confirmClose,
    busy,
    error,
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
