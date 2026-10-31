import { LOCAL_SLOT, type Slot } from "./slots";

/** 注册表持有的东西的最小契约：能自我销毁即可。 */
export interface Disposable {
  dispose(): void;
}

/**
 * 按槽位持有 RPC 客户端，并记录「当前活动槽位」。
 *
 * **为什么需要它**（spec §4.6「前端侧的单客户端假设」）：一个进程一个
 * `app-server` 的设计在引入远端项目后不再成立——本地槽位 + N 个远端项目槽位
 * 各自有一条独立连接。单客户端写法会让三件事出错：
 *
 * 1. 事件订阅：所有槽位共用 `app-server://message`，A 项目的逐字流会落进
 *    B 项目的视图（事件名带槽位后解决，本类只管实例归属）。
 * 2. 重连：切回前台只重建「当前」那条，其它被系统掐断的连接永久半死。
 * 3. dispose：换客户端时漏掉某条旧连接，旧连接的事件会与新的重复应用。
 *
 * 本类把「按槽位归属 + 严格 dispose」收在一处，App 只负责在合适的时机调用。
 */
export class ClientRegistry<T extends Disposable> {
  private readonly slots = new Map<Slot, T>();
  private activeSlot: Slot = LOCAL_SLOT;

  /** 取某槽位的客户端；没有就是 undefined（远端未连接时是常态）。 */
  get(slot: Slot): T | undefined {
    return this.slots.get(slot);
  }

  /** 当前活动槽位。默认 `local`。 */
  active(): Slot {
    return this.activeSlot;
  }

  /** 切换活动槽位。**不**校验存在性：切到一个还没连上的远端项目是合法的中间态。 */
  setActive(slot: Slot): void {
    this.activeSlot = slot;
  }

  /** 当前活动槽位的客户端。 */
  current(): T | undefined {
    return this.slots.get(this.activeSlot);
  }

  /**
   * 登记客户端，**先销毁同槽位的旧实例**。
   *
   * 不销毁旧实例的后果是具体的：Tauri 事件监听不像关闭的 socket 那样会停，
   * 未 dispose 的旧 transport 会与新的一起消费每条帧，append-only 的
   * `item/delta` 于是被应用两遍——流式文本重复，直到 `item/completed` 按 id
   * 整条覆盖才「收敛」。
   */
  put(slot: Slot, client: T): T {
    const previous = this.slots.get(slot);
    if (previous && previous !== client) previous.dispose();
    this.slots.set(slot, client);
    return client;
  }

  /** 摘掉并销毁某槽位。用于用户主动断开远端项目。 */
  remove(slot: Slot): void {
    const client = this.slots.get(slot);
    if (!client) return;
    this.slots.delete(slot);
    client.dispose();
  }

  /**
   * 摘掉并销毁除本地之外的所有槽位。
   *
   * 关掉远端开关时用（spec §5.4）：留着这些客户端的话，它们的 transport 仍在
   * 监听事件，而对应的进程已经被杀——用户看到的是「关了开关，远端项目的流还在
   * 动一下」。活动槽位若正好是远端，调用方需要自行回退（见 `App`）。
   */
  removeRemotes(): void {
    for (const slot of [...this.slots.keys()]) {
      if (slot !== LOCAL_SLOT) this.remove(slot);
    }
  }

  /** 已登记的全部槽位，用于「遍历所有连接」的操作（如切回前台重连）。 */
  keys(): Slot[] {
    return [...this.slots.keys()];
  }

  /**
   * 全局只读信息的**唯一**来源：永远取本地槽位。
   *
   * `build/info` 与 `flags/list` 都属此类（spec §5.2）：远端在跑自己的二进制，
   * 它报的 build kind 与 flag 值描述的是**那台机器**。拿远端的答案去渲染本机的
   * 「实验功能」面板，会得到一个恰好看起来合理、但来源完全错误的值。
   */
  forGlobalReads(): T | undefined {
    return this.slots.get(LOCAL_SLOT);
  }

  /**
   * 销毁全部并清空。组件卸载时用。
   *
   * 逐个容错：一个 transport 在 dispose 里抛错，不得让其余连接带着监听泄漏
   * 下去（那正是「重连后内容重复」的成因）。清空先做完，再抛第一个错误。
   */
  disposeAll(): void {
    const clients = [...this.slots.values()];
    this.slots.clear();
    const failures: unknown[] = [];
    for (const client of clients) {
      try {
        client.dispose();
      } catch (error) {
        failures.push(error);
      }
    }
    if (failures.length > 0) throw failures[0];
  }
}
