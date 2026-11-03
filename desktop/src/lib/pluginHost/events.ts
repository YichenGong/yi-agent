/**
 * 事件中心：修订号、快照优先、按绑定清理。
 *
 * 协议 spec §7 的两条硬规则都在这里落地：
 *
 * - 每个 topic 有单调递增的 `revision`。**没有缺口才是正常**。
 * - 续订时如果 `after` 与当前修订不连续，必须先取快照，再以快照的修订号
 *   续订。**不允许**用"我丢了 N 个事件"来推导状态——丢的内容未知，推出来
 *   的是猜的。
 *
 * 清理（T18-h）用计数断言：`clear()` 返回被移除的订阅数，`subscriberCount()`
 * 在清理后必须是 0。不靠"没抛错"证明清理成功。
 */

import type { BridgeEvent, BridgeError, Snapshot } from "./types";

export interface SubscribeHandle<T = unknown> {
  /** 本次订阅的起始修订号（有快照时为快照的修订号）。 */
  revision: number;
  /** 补发的一致性快照；`after` 连续时为 `null`。 */
  snapshot: Snapshot<T> | null;
  unsubscribe(): void;
}

export interface EventHubOptions {
  /** 取某个 topic 的权威快照。宿主不提供时，缺口就是不可恢复的。 */
  snapshot?: (topic: string) => Promise<Snapshot<unknown>>;
  /** 每个 topic 最多保留多少个事件用于窗口内补发。 */
  maxBufferedPerTopic?: number;
}

interface TopicState {
  revision: number;
  buffered: BridgeEvent[];
  subscribers: Set<{ handler: (event: BridgeEvent) => void }>;
}

const DEFAULT_MAX_BUFFERED = 64;

export class BridgeSubscriptionError extends Error {
  constructor(readonly error: BridgeError) {
    super(`${error.kind}: ${error.detail}`);
    this.name = "BridgeSubscriptionError";
  }
}

export class EventHub {
  private readonly topics = new Map<string, TopicState>();
  private readonly maxBuffered: number;

  constructor(private readonly options: EventHubOptions = {}) {
    this.maxBuffered = options.maxBufferedPerTopic ?? DEFAULT_MAX_BUFFERED;
  }

  /** 当前修订号；从未发布过的 topic 是 0。 */
  revision(topic: string): number {
    return this.topics.get(topic)?.revision ?? 0;
  }

  /** 活跃订阅数。清理测试用它断言，而不是看有没有异常。 */
  subscriberCount(topic?: string): number {
    if (topic !== undefined) return this.topics.get(topic)?.subscribers.size ?? 0;
    let total = 0;
    for (const state of this.topics.values()) total += state.subscribers.size;
    return total;
  }

  /** 宿主侧发布。返回发布后的修订号。 */
  publish<T>(topic: string, payload: T): number {
    const state = this.ensure(topic);
    state.revision += 1;
    const event: BridgeEvent<T> = { topic, revision: state.revision, payload };
    state.buffered.push(event as BridgeEvent);
    if (state.buffered.length > this.maxBuffered) {
      state.buffered.splice(0, state.buffered.length - this.maxBuffered);
    }
    // 订阅者可能 unsubscribe，先快照集合再遍历。
    for (const subscriber of [...state.subscribers]) {
      subscriber.handler(event as BridgeEvent);
    }
    return state.revision;
  }

  /**
   * 续订。
   *
   * `after` 等于当前修订 → 直接订阅（连续，无需补发）。
   * 否则 → 先取快照，把快照交给调用方，再按快照的修订号订阅，并补发
   * 快照之后已经产生的事件。
   */
  async subscribe<T>(
    topic: string,
    after: number,
    handler: (event: BridgeEvent<T>) => void,
  ): Promise<SubscribeHandle<T>> {
    const state = this.ensure(topic);

    if (after === state.revision) {
      const entry = { handler: handler as (event: BridgeEvent) => void };
      state.subscribers.add(entry);
      return {
        revision: after,
        snapshot: null,
        unsubscribe: () => {
          state.subscribers.delete(entry);
        },
      };
    }

    if (!this.options.snapshot) {
      throw new BridgeSubscriptionError({
        kind: "RecoveryRequired",
        detail: `topic "${topic}" is at revision ${state.revision} but the subscriber asked for ${after} and no snapshot source is configured`,
      });
    }

    const snapshot = (await this.options.snapshot(topic)) as Snapshot<T>;
    if (snapshot.topic !== topic) {
      throw new BridgeSubscriptionError({
        kind: "Io",
        detail: `snapshot for "${topic}" came back labelled "${snapshot.topic}"`,
      });
    }

    // 快照期间可能有事件到达；只补发快照之后的部分。
    const gap = state.buffered.filter((event) => event.revision > snapshot.revision);
    const entry = { handler: handler as (event: BridgeEvent) => void };
    state.subscribers.add(entry);
    if (snapshot.revision > state.revision) {
      // 快照比内存里的修订更新（例如宿主重启后恢复），以快照为准。
      state.revision = snapshot.revision;
    }
    for (const event of gap) {
      handler(event as BridgeEvent<T>);
    }

    return {
      revision: snapshot.revision,
      snapshot,
      unsubscribe: () => {
        state.subscribers.delete(entry);
      },
    };
  }

  /** 移除全部订阅，返回移除数量。旧通道失效时必须调用。 */
  clear(): number {
    let removed = 0;
    for (const state of this.topics.values()) {
      removed += state.subscribers.size;
      state.subscribers.clear();
    }
    return removed;
  }

  /** 测试与诊断用：缓冲区里保留的修订号。 */
  bufferedRevisions(topic: string): number[] {
    return (this.topics.get(topic)?.buffered ?? []).map((event) => event.revision);
  }

  private ensure(topic: string): TopicState {
    let state = this.topics.get(topic);
    if (!state) {
      state = { revision: 0, buffered: [], subscribers: new Set() };
      this.topics.set(topic, state);
    }
    return state;
  }
}
