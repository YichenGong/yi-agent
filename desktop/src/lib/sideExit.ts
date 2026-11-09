/**
 * 退出旁问的意图账本（spec §4.3、§6.2、§6.4）。
 *
 * 退出是**破坏性**的：用户按返回的那一刻，本地正文已经丢弃，服务端环境也必须
 * 被销毁。但销毁是异步的、可能失败、可能断网——于是这里只保留一件最小的东西：
 * 「哪个宿主的哪个票据还没被确认销毁」。它**不**保存正文、不保存首问、不保存
 * 可以让用户接着聊的东西。
 *
 * 三条规则决定了整个设计：
 *
 * 1. **只有 `closed` 才解除重入屏障。** `closing` 是「意图已记录」，`failed` 是
 *    「清理没成功」。把 RPC 已返回当成环境已销毁，会让用户在旧环境还在排空时
 *    建出第二套，而宿主保证同一父最多一个环境——于是第二套要么被拒，要么把
 *    用户送进一个马上就要被拆掉的环境。
 * 2. **票据是精确的三元组。** `(hostInstanceId, parentThreadId, entryId)` 一起
 *    定位一代。只用 parent 会在「旧票重试」时杀掉重新进入建出的新环境；只用
 *    entry 则无法拒绝另一个宿主上恰好同名的票据。
 * 3. **旧宿主不可达时不能假装成功。** 宿主换了一代（重连到新进程），旧票据
 *    永远不可能被确认：删掉它、明确告诉用户「旧宿主清理无法核实」，而不是拿
 *    新宿主的连接去发旧 exit——那只会关掉新宿主上一个无关的环境。
 */

import type { SideExitParams, SideExitStateWire } from "./protocol";
import type { SideHostBinding } from "./sideConversation";

/** 一条退出意图的状态。 */
export type SideExitPhase =
  /** 已记录，尚未发出（断网、或还没轮到）。 */
  | "pending"
  /** 有一次 exit 在途。 */
  | "inflight"
  /** 宿主明确说清理未完成（`failed`），或请求本身失败：可重试。 */
  | "failed";

export interface SideExitRecord {
  target: SideExitParams;
  phase: SideExitPhase;
  /** 最近一次失败的原因（给重试按钮用）。 */
  message: string | null;
}

/**
 * 票据键。
 *
 * 用 `JSON.stringify` 而不是 `join(":")`：`entryId` 是服务端生成的，但
 * `parentThreadId` 是用户可选的会话 id，含冒号完全可能。拼接会让两条不同的
 * 意图撞成同一个键，于是其中一条永远发不出去。
 */
export function sideExitKey(t: SideExitParams): string {
  return JSON.stringify([t.hostInstanceId, t.parentThreadId, t.entryId]);
}

/** 三个身份都非空才算一条可发送的意图。 */
function isComplete(t: SideExitParams): boolean {
  return (
    t.hostInstanceId !== "" && t.parentThreadId !== "" && t.entryId !== ""
  );
}

export class SideExitCoordinator {
  private readonly onChange: () => void;
  private readonly records = new Map<string, SideExitRecord>();
  /** 正在等待确认的宿主已被替换：这些提示只出现一次。 */
  private notice: string | null = null;
  private disposed = false;

  constructor(onChange: () => void) {
    this.onChange = onChange;
  }

  /** 记一条退出意图。重复记录同一条是幂等的。 */
  record(target: SideExitParams): void {
    if (this.disposed) return;
    if (!isComplete(target)) return;
    const key = sideExitKey(target);
    if (this.records.has(key)) return;
    this.records.set(key, { target: { ...target }, phase: "pending", message: null });
    this.notify();
  }

  /**
   * 把一条失败记录重新放回待发队列（用户点了「重试」）。
   *
   * 只重置**这张票**：别的宿主、别的父上的失败不受影响，也不会顺手重试一堆
   * 已经明确失败的东西——那会让一次点击变成一串后台请求。
   */
  retry(target: SideExitParams): void {
    if (this.disposed) return;
    const record = this.records.get(sideExitKey(target));
    if (record === undefined || record.phase !== "failed") return;
    record.phase = "pending";
    record.message = null;
    this.notify();
  }

  /**
   * 这个 `(宿主, 父)` 上是否还有未确认的退出。
   *
   * 为真期间不能为同一父创建新环境：旧环境还可能占着宿主上的唯一性槽位，
   * 而新建出来的环境会立刻被旧环境的排空流程撞上。
   */
  blocked(hostInstanceId: string, parentThreadId: string): boolean {
    for (const record of this.records.values()) {
      if (
        record.target.hostInstanceId === hostInstanceId &&
        record.target.parentThreadId === parentThreadId
      ) {
        return true;
      }
    }
    return false;
  }

  /** 未确认的意图（只读快照，供 UI 显示数量与重试入口）。 */
  pending(): SideExitRecord[] {
    return [...this.records.values()].map((r) => ({
      target: { ...r.target },
      phase: r.phase,
      message: r.message,
    }));
  }

  /** 最近一条一次性的提示（例如「旧宿主清理无法核实」）。读后不清。 */
  lastNotice(): string | null {
    return this.notice;
  }

  clearNotice(): void {
    if (this.notice === null) return;
    this.notice = null;
    this.notify();
  }

  /**
   * 补发这些宿主的退出。
   *
   * `binding` 是**当前**连接上的宿主绑定。只处理同一宿主实例的记录：
   * 旧实例的票据在新实例上没有任何意义，发给它只会误关别人。
   *
   * 三种不做的情况都是刻意的：
   * - 宿主不支持本次生命周期协议：没有 `side/exit` 可发，如实标记无法核实。
   * - 连接已断：保留意图，等下一次重连（不在这里排无上限的重试队列）。
   * - 本设备是 `observe`：退出共享环境是 Control 权限，只读端的本地离开
   *   不该关闭别人正在用的环境（spec §7）。
   */
  async flush(binding: SideHostBinding): Promise<void> {
    if (this.disposed) return;
    const mine = [...this.records.values()].filter(
      (r) => r.target.hostInstanceId === binding.instanceId,
    );
    if (mine.length === 0) return;
    if (!binding.supported || !binding.snapshotLifecycle) {
      for (const record of mine) this.drop(sideExitKey(record.target));
      this.notice = "旧宿主不支持退出协议，清理无法核实";
      this.notify();
      return;
    }
    if (binding.scope === "observe") {
      // 只读端的退出只清本地视图：它没有关闭共享环境的权限。
      for (const record of mine) this.drop(sideExitKey(record.target));
      this.notify();
      return;
    }
    if (!binding.connected) return;
    for (const record of mine) {
      const key = sideExitKey(record.target);
      if (record.phase === "inflight") continue;
      record.phase = "inflight";
      record.message = null;
      this.notify();
      try {
        const result = await binding.client.request<{
          hostInstanceId?: unknown;
          parentThreadId?: unknown;
          entryId?: unknown;
          state?: unknown;
        }>("side/exit", record.target);
        if (this.disposed) return;
        const state = result.state;
        if (!this.sameTarget(record.target, result)) {
          // 回包描述的不是这张票据：不能凭它放行重入。
          record.phase = "failed";
          record.message = "宿主返回了不匹配的退出票据";
          this.notify();
          continue;
        }
        if (state === "closed") {
          this.drop(key);
          this.notify();
          continue;
        }
        if (state === "failed") {
          record.phase = "failed";
          record.message = "宿主未能完成清理，请重试";
          this.notify();
          continue;
        }
        // `closing`：意图已记录，资源还在排空。保留屏障，等下一次对账。
        record.phase = "pending";
        record.message = null;
        this.notify();
      } catch (e) {
        if (this.disposed) return;
        record.phase = "failed";
        record.message = describeExitFailure(e);
        this.notify();
      }
    }
  }

  /**
   * 宿主换代（重连到另一个进程实例）。
   *
   * 旧实例的票据在新实例上无法确认：删掉它们，并明确告诉用户「清理无法核实」。
   * 不能拿新连接去补发旧 exit——新宿主的票据表里没有它，最坏会命中一个恰好
   * 同名的票据，把另一端正在用的环境关掉。
   */
  hostReplaced(oldHostInstanceId: string): void {
    if (this.disposed) return;
    let dropped = false;
    for (const [key, record] of [...this.records]) {
      if (record.target.hostInstanceId !== oldHostInstanceId) continue;
      this.records.delete(key);
      dropped = true;
    }
    if (!dropped) return;
    this.notice = "旧宿主清理无法核实";
    this.notify();
  }

  dispose(): void {
    this.disposed = true;
    this.records.clear();
  }

  private drop(key: string): void {
    this.records.delete(key);
  }

  private sameTarget(
    target: SideExitParams,
    result: { hostInstanceId?: unknown; parentThreadId?: unknown; entryId?: unknown },
  ): boolean {
    return (
      result.hostInstanceId === target.hostInstanceId &&
      result.parentThreadId === target.parentThreadId &&
      result.entryId === target.entryId
    );
  }

  private notify(): void {
    if (this.disposed) return;
    this.onChange();
  }
}

/** 回包里的 `state` 是不是我们认识的三个值之一（供调用方断言）。 */
export function isSideExitState(v: unknown): v is SideExitStateWire {
  return v === "closing" || v === "closed" || v === "failed";
}

function describeExitFailure(error: unknown): string {
  if (error instanceof Error && error.message !== "") return error.message;
  return "退出请求失败，请重试";
}
