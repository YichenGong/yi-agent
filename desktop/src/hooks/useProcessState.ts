import { useEffect, useState } from "react";
import { type ProcessInfo, type ProcessListResult, type ProcessReadResult } from "../lib/protocol";
import type { RpcClient } from "../lib/rpc";
import type { ThreadStore } from "../lib/threadStore";

/**
 * 会话级后台进程列表与详情态（右侧详情栏的进程 Tab 消费）。
 *
 * 归属是这里唯一的硬约束：列表、被打开的进程 id、增量输出都必须答得出「这是
 * **哪条**会话的进程」。故 `currentId` 由调用方注入，而非另起一份影子——它是
 * 「此刻选中谁」在渲染期的唯一事实来源，详情轮询的 effect 依赖它重建、列表刷新
 * 也以它为参数。
 *
 * `rpc` 是「取客户端」的 getter：未连接时返回 null，动作**静默返回**。这条语义
 * 逐字保留自搬移前（原为 `const c = clientRef.current; if (!c) return;`）：进程
 * 是附属视图，断线读不到不该在对话里留一条红字，更不该漏出一个没人 catch 的
 * rejection（故不学 `pluginCall` 那套 `Promise.reject` 接缝）。
 *
 * `store` 与 `force` 都是「回包归属」的判据，不是本 hook 的状态：后台 thread 的
 * `process/updated` 通知也会打到这里，只有 `store.currentId === threadId` 的回包
 * 才允许落盘（否则会改到用户正在看的那一列），而列表一变就得 `force` 一下让
 * store 视图重读。二者都是 App 层的东西，故由调用方注入。
 *
 * 详情 Tab 的开合（`detailTab`）与「切走进程 Tab 即关详情」不属于进程本身：前者
 * 是会话关注点的状态，由 `App.tsx` 在切 Tab 的现场自行收起详情（见那几处调用点）。
 */
export function useProcessState({
  rpc,
  store,
  force,
  currentId,
}: {
  /** 取当前的 RPC 客户端；未连接即 null（各动作据此静默返回）。 */
  rpc: () => RpcClient | null;
  /** 会话存储：`refreshProcesses` 用它判断回包是否属于当前对话。 */
  store: ThreadStore;
  /** App 的强制重渲染 tick（进程列表变化后 store 视图需重读）。 */
  force: (updater: (v: number) => number) => void;
  /** 当前会话 id：详情轮询与列表刷新都以它为参数。 */
  currentId: string | null;
}) {
  // 当前会话的后台进程列表与详情态。列表由 process/list 拉取、process/updated 提示
  // 重拉；详情打开时以 500ms 增量轮询 process/read（只在详情打开期间）。
  const [processes, setProcesses] = useState<ProcessInfo[]>([]);
  const [openProcessId, setOpenProcessId] = useState<string | null>(null);
  const [processOutput, setProcessOutput] = useState<{
    stdout: string;
    stderr: string;
    truncated: boolean;
  } | null>(null);

  // 进程详情打开期间，以 500ms 增量拉取 stdout/stderr；关闭或终态即停。
  // 光标是本次订阅的局部量，进程/会话一变就随 effect 重建。
  useEffect(() => {
    if (!currentId || !openProcessId) return;
    const c = rpc();
    if (!c) return;
    let cursor = 0;
    let cancelled = false;
    let timer: number | undefined;

    const tick = async () => {
      try {
        const r = await c.request<ProcessReadResult>("process/read", {
          thread_id: currentId,
          process_id: openProcessId,
          cursor,
        });
        if (cancelled) return;
        cursor = r.next_cursor;
        setProcessOutput({
          stdout: r.stdout,
          stderr: r.stderr,
          truncated: r.truncated,
        });
        // 终态即停:不再有新增输出,退出码已经由列表侧展示。
        const state = r.status?.state;
        if (state === "exited" || state === "killed" || state === "failed_to_start") {
          return;
        }
      } catch {
        // 读失败(例如进程刚被移除):不打断 UI,等下一次 tick 或用户关闭。
      }
      if (!cancelled) timer = window.setTimeout(tick, 500);
    };
    void tick();
    return () => {
      cancelled = true;
      if (timer !== undefined) window.clearTimeout(timer);
    };
  }, [currentId, openProcessId]);

  /**
   * 取某对话当前的后台进程列表。
   *
   * 失败即放弃:与子 agent 暂留区同类,附属视图读不到就保持上一次的值(或空),
   * 绝不因为它把对话打断。未知 thread 服务端返回空表,不会走到 catch。
   */
  const refreshProcesses = async (threadId: string) => {
    const c = rpc();
    if (!c) return;
    try {
      const r = await c.request<ProcessListResult>("process/list", { thread_id: threadId });
      // 只写当前对话:后台 thread 的状态通知不应改到用户正在看的这一列。
      if (store.currentId === threadId) {
        setProcesses(r.processes);
        force((v) => v + 1);
      }
    } catch {
      // 附属视图:读失败不改动已有内容。
    }
  };

  return {
    // state：面板直接消费的三项。
    processes,
    openProcessId,
    processOutput,
    // actions / setters：App 的 JSX（开合详情、轮询写入）、`selectThread` 与
    // 生命周期 effect 都要用。
    setOpenProcessId,
    setProcessOutput,
    refreshProcesses,
  };
}
