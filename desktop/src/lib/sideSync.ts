/**
 * 旁支的快照对账与提交（spec §8.2、§8.3）。
 *
 * 三件事，每件都有一条**具体的**失败模式，所以必须显式写出来而不是「尽力而为」：
 *
 * 1. **先订阅再分页**。反过来的话，分页与订阅之间产生的那几帧就永久丢了，而
 *    用户的正文会停在一个看似合理、实际缺了一段的中间态。
 * 2. **快照不是「最新的」，只是「一致的」**。分页期间到达的帧按 seq 缓存，
 *    装完基线之后只应用 `seq > watermark` 的那部分，且必须**连续**。取
 *    `max(seq)` 跳洞会让缺掉的那段永久消失。
 * 3. **提交的结果可能不确定**。断线时请求可能已受理、也可能没到。此时不生成
 *    新 `requestId`、不换新旁支，只用同一个 id 对账或重试——否则同一个问题会
 *    被回答两次。
 */

import { formatError } from "./errorMessage";
import type { Item, SideReadItemsResult, SideTurnStartResult } from "./protocol";
import type {
  SideDraftSnapshot,
  SideHostBinding,
  SideHostKey,
  SideNotification,
  SideConversationStore,
  SideSubmitReceipt,
  SideTarget,
} from "./sideConversation";

/** 通知缓存上限（spec §8.2：256 帧 / 2 MiB）。 */
export const SIDE_CACHE_MAX_FRAMES = 256;
export const SIDE_CACHE_MAX_BYTES = 2 * 1024 * 1024;

/**
 * 尾帧探测间隔（spec §8.2）。
 *
 * 连接活着也可能丢最后一帧（服务端 panic、relay 丢包），而「永远停在 running」
 * 是最难被用户描述、也最难被我们复现的 bug。每 5 秒比一次 `last_seq` 很便宜。
 */
export const SIDE_TAIL_POLL_MS = 5000;

/** 单页请求上限：服务端按字节切页，但一次抓太多页会长时间占住内存。 */
export const SIDE_MAX_PAGES = 64;

/** 控制器已经拆掉时的拒绝原因。不写 store，只回给调用方。 */
const SIDE_DISPOSED_MESSAGE = "旁问连接已关闭";

export interface SideSyncDeps {
  binding: SideHostBinding;
  store: SideConversationStore;
  /** 订阅父会话（spec §8.2：所有旁支帧以父 threadId 为订阅过滤 key）。 */
  subscribeParent: (parentId: string) => Promise<void>;
  clock: () => number;
  /** 通知到达后的重渲染钩子（store 更新本身不驱动 React）。 */
  onChange?: () => void;
}

/** 一帧里能算出「它占多少内存」的最小近似：线上形状就是 JSON。 */
function frameBytes(n: SideNotification): number {
  try {
    return JSON.stringify(n).length;
  } catch {
    // 理论上不可达（线上形状一定能序列化）。真出问题也不能因此丢掉缓存上限
    // 的账，按最大单帧预算记一笔即可。
    return 128 * 1024;
  }
}

interface PendingRange {
  /** 从 `seq` 起连续。 */
  from: number;
  frames: SideNotification[];
  bytes: number;
}

/**
 * 一个旁支的同步控制器。
 *
 * 一个宿主可以有多条旁支，但同一个 `(hostKey, sideId, generation)` 同一时刻只
 * 允许一个对账周期在跑：两轮分页交错会把两段不同时刻的正文拼进同一份基线。
 * `reconciling` 就是这个互斥。
 */
export class SideSyncController {
  private readonly binding: SideHostBinding;
  private readonly store: SideConversationStore;
  private readonly subscribeParent: (parentId: string) => Promise<void>;
  private readonly clock: () => number;
  private readonly onChange?: () => void;

  /** 正在对账的 parent id（同时只有一个）。 */
  private reconciling: string | null = null;
  /** 进行中的对账必须能被打断：dispose 或换代时旧结果一律不得落地。 */
  private abort: AbortController | null = null;
  private tailTimer: ReturnType<typeof setInterval> | null = null;
  private visibleParent: string | null = null;
  /** 一轮探测还在路上：不再叠一轮，免得慢网下堆积请求。 */
  private polling = false;
  /** 上一次全量对账完成的时刻。刚对完就不必再探测一遍。 */
  private lastReconciledAt: number | null = null;
  /** 对账期间缓存的通知，按 sideId 分组。 */
  private readonly cache = new Map<string, PendingRange>();
  /**
   * 对账期间缓存溢出的 side。
   *
   * 溢出发生在 `installSnapshot` 还没装上基线的时候，那时视图可能还不存在，
   * `setSync` 会静默无效果——溢出信号于是丢失，基线装完照样标 `synced`，用户
   * 得到一份缺口未知的正文。这个集合把信号留到装基线那一刻再消费。
   */
  private readonly overflowed = new Set<string>();
  /** 本轮对账期间发现必须全量重读，等互斥释放后再排下一轮。 */
  private readonly needsReconcile = new Set<string>();
  private readonly unsubscribe: Array<() => void> = [];
  private disposed = false;

  constructor(deps: SideSyncDeps) {
    this.binding = deps.binding;
    this.store = deps.store;
    this.subscribeParent = deps.subscribeParent;
    this.clock = deps.clock;
    this.onChange = deps.onChange;
  }

  get hostKey(): SideHostKey {
    return this.binding.key;
  }

  /**
   * 绑定在这条控制器上的客户端。
   *
   * 重连会换一条连接而复用同一个 `hostKey`：调用方据此判断「该不该重建」。
   * 继续往一条已 dispose 的客户端发请求，得到的是永远不返回的 Promise。
   */
  get client() {
    return this.binding.client;
  }

  /**
   * 全量对账：订父 → list → 分页 → 装基线 → 应用缓存 → 标 synced。
   *
   * `parentId` 是这一轮的目标。列表为空的含义是「没有旁支」（可以清掉本地
   * 占位），而**网络失败**的含义是「不知道」——两者绝不能都当成「已关闭」，
   * 那会让用户在断网时看到旁支被静默丢弃。
   */
  async reconcile(parentId: string): Promise<void> {
    if (this.disposed) return;
    if (this.reconciling === parentId) return;
    this.reconciling = parentId;
    const controller = new AbortController();
    this.abort?.abort();
    this.abort = controller;
    const generationAtStart = this.generationOf(parentId);

    try {
      await this.subscribeParent(parentId);
      if (controller.signal.aborted) return;

      let list: { sides: unknown };
      try {
        list = await this.binding.client.request<{ sides: unknown }>(
          "side/list",
          { parentThreadId: parentId },
        );
      } catch (error) {
        // 网络失败 ≠ 已关闭：保留占位视图并标记未同步，禁发。
        if (isNetworkFailure(error)) {
          this.markUnsynced(parentId);
          return;
        }
        throw error;
      }
      if (controller.signal.aborted) return;

      const sides = parseSideList(list);
      if (sides.length === 0) {
        // list 明确为空：这个父上没有旁支，清掉旧的临时视图。宿主重启后
        // sideId 不会复用，留着只会让用户点开一个必然 side_not_found 的壳。
        this.store.clearParent(this.binding.key, parentId);
        this.onChange?.();
        return;
      }

      for (const summary of sides) {
        if (controller.signal.aborted) return;
        if (generationAtStart !== null && summary.generation < generationAtStart) {
          // 对账期间已经看到更新的一代：这一份快照描述的是一段已经翻篇的历史。
          continue;
        }
        await this.installSnapshot(parentId, summary.side_id, summary.generation, controller);
      }
    } finally {
      if (this.abort === controller) this.abort = null;
      if (this.reconciling === parentId) this.reconciling = null;
      this.lastReconciledAt = this.clock();
      // 对账期间自己发现的缺口（缓存溢出、缓存内缺号）：**必须等这轮结束后**
      // 再排下一轮。在 `reconciling` 还占着的时候调用 `reconcile` 会被互斥
      // 直接挡掉，补救请求就静默消失了。
      if (this.needsReconcile.delete(parentId) && !controller.signal.aborted) {
        this.scheduleFullReconcile(parentId);
      }
    }
  }

  /**
   * 分页抓完一个 side，装基线，再把期间缓存的帧按序应用。
   *
   * 分页用固定 token：每一页都带同一个 `snapshot_token`，只换 `cursor`。换 token
   * 意味着换快照，两页拼起来就是两个时刻的正文。
   */
  private async installSnapshot(
    parentId: string,
    sideId: string,
    generation: number,
    controller: AbortController,
  ): Promise<void> {
    const items: Item[] = [];
    let first: SideReadItemsResult | null = null;
    let token: string | undefined;
    let cursor: string | undefined;
    let pages = 0;

    while (pages < SIDE_MAX_PAGES) {
      if (controller.signal.aborted) return;
      const page = await this.binding.client.request<SideReadItemsResult>(
        "side/readItems",
        {
          sideId,
          ...(token === undefined ? {} : { snapshotToken: token }),
          ...(cursor === undefined ? {} : { cursor }),
        },
      );
      pages += 1;
      if (first === null) first = page;
      items.push(...page.items);
      if (page.next_cursor === null) break;
      token = page.snapshot_token;
      cursor = page.next_cursor;
      if (pages === SIDE_MAX_PAGES) {
        // 没抓完就不能装半份基线：半份 + 通知缓存会得到一份缺尾的正文。
        throw new SideSyncError("snapshot_incomplete", "side snapshot exceeded page budget");
      }
    }
    if (controller.signal.aborted || first === null) return;

    this.store.installBaseline(this.binding.key, sideId, first, items);
    this.store.adopt(this.binding.key, sideId, parentId, generation);
    // `synced` 只在缓存也接得上时才算数：基线本身一致，但缓存若有洞/溢出，
    // 合起来的视图仍是缺的，标 `synced` 会让发送放行到一份不完整的历史上。
    const contiguous = this.replayCached(parentId, sideId, generation, first.snapshot_seq);
    this.store.setSync(this.binding.key, sideId, contiguous ? "synced" : "unsynced");
    this.onChange?.();
  }

  /**
   * 到达的旁支帧。
   *
   * 分两类：对账在途时进缓存（按 seq 有序、受 256 帧 / 2 MiB 约束），否则直接
   * 应用。两种情形都先做「连续」检查——缺号必须触发全量重读，不能跳过去。
   */
  accept(n: SideNotification): void {
    if (this.disposed) return;
    if (n.params.side_id === "") return;
    // 只在「正在对账的是同一个父」时缓存：缓存按 sideId 分桶，跨父混进来
    // 的帧永远等不到对应的一次 `replayCached`。
    if (this.reconciling === n.params.parent_thread_id) {
      this.cacheFrame(n);
      return;
    }
    if (this.reconciling !== null) {
      // 另一条父的对账在跑：这一帧等下一轮（下方 `applyOne` 会按需再排一次）。
      return;
    }
    this.applyOne(n);
  }

  /** 视图可见性变化：可见时保持尾帧探测，后台停掉。 */
  setVisible(parentId: string | null): void {
    this.visibleParent = parentId;
    if (this.disposed) return;
    if (parentId === null) {
      this.stopTailPoll();
      return;
    }
    this.startTailPoll(parentId);
  }

  dispose(): void {
    this.disposed = true;
    this.abort?.abort();
    this.abort = null;
    this.stopTailPoll();
    for (const off of this.unsubscribe.splice(0)) off();
    this.cache.clear();
  }

  /**
   * 提交一个问题。
   *
   * 只在已同步态放行：未同步时提交会与一个还没装上的基线打架，用户看到自己的
   * 问题消失（因为基线把它覆盖了）。
   *
   * 返回值是**收据**而不是 boolean：成功、明确拒绝、应答丢失三种结果的后续动作
   * 完全不同（清稿 / 还稿并报错 / 保留 pending 对账），压成 true/false 就会丢掉
   * 用户正在写的东西（spec §5.2）。
   *
   * 失败时把 pending 置为 `uncertain` 并**保留同一个 requestId**：重连后先用
   * `accepted_requests` 对账，或用同一个 id 重试。绝不生成新 id。
   */
  async submit(
    sideId: string,
    text: string,
    clientRequestId: string,
  ): Promise<SideSubmitReceipt> {
    // disposed 一律不写 store：控制器已经被拆掉，此时写进去的是另一个视图世界
    // 的状态，调用方也没人接收 onChange。
    if (this.disposed) return { kind: "rejected", requestId: clientRequestId, message: SIDE_DISPOSED_MESSAGE };
    const view = this.store.read(this.binding.key, sideId);
    if (!view) {
      return { kind: "rejected", requestId: clientRequestId, message: "这条旁问已经不存在了" };
    }
    if (view.sync !== "synced") {
      // 请求还没发出去，所以不是「不确定」：调用方可以放心保留原稿并提示重试。
      return {
        kind: "rejected",
        requestId: clientRequestId,
        message: "旁问还没同步完成，暂时不能发送",
      };
    }
    // 目标与版本都在**发请求之前**抓齐：回包时再读 store 可能已经换了一代，
    // 或者用户已经改了草稿。
    const snapshot: SideDraftSnapshot & { requestId: string } = {
      target: {
        kind: "side",
        hostKey: this.binding.key,
        parentId: view.parentId,
        sideId,
        generation: view.generation,
      },
      text,
      draftVersion: view.draftVersion,
      requestId: clientRequestId,
    };
    if (view.draft.trim() === "" && text !== "") {
      // 自动首问走这里：问题正文不是从输入框来的，得先落进草稿，回包才知道要
      // 清掉的是哪一版。**草稿非空时不覆盖**——那是用户自己在写的东西，一句
      // 后台发起的首问没有资格把它顶掉（spec §5.2）。
      this.store.installQuestion(
        this.binding.key,
        sideId,
        text,
        snapshot.draftVersion + 1,
      );
      snapshot.draftVersion += 1;
    }
    this.store.setPending(this.binding.key, sideId, {
      requestId: clientRequestId,
      text,
      draftVersion: snapshot.draftVersion,
      state: "submitting",
    });
    this.onChange?.();
    try {
      await this.binding.client.request<SideTurnStartResult>("side/turnStart", {
        sideId: snapshot.target.sideId,
        text: snapshot.text,
        clientRequestId: snapshot.requestId,
      });
      this.clearSubmittedIfCurrent(sideId, snapshot);
      return { kind: "accepted", requestId: snapshot.requestId };
    } catch (error) {
      if (!isNetworkFailure(error)) {
        // 服务端明确拒绝：这不是「不确定」。不保留 pending，否则会显示一个永远
        // 转圈的「发送中」。
        this.store.setPending(this.binding.key, sideId, null);
        const message = formatError(error);
        this.rejectSubmission(sideId, snapshot, message);
        this.onChange?.();
        return { kind: "rejected", requestId: snapshot.requestId, message };
      }
      // 结果不确定：保留同 id/text，标记待确认。
      const current = this.store.read(this.binding.key, sideId);
      if (
        current?.generation === snapshot.target.generation &&
        current.pending?.requestId === snapshot.requestId
      ) {
        this.store.setPending(this.binding.key, sideId, {
          ...current.pending,
          state: "uncertain",
        });
      }
      this.onChange?.();
      return { kind: "uncertain", requestId: snapshot.requestId };
    }
  }

  /**
   * 断线重连后确认一次未确定提交。
   *
   * 用 `accepted_requests` 判定：服务端受理过就会在那里有 `requestId -> turnId`。
   * 确认了就清 pending；没有则用**同一个** id 重试一次。
   *
   * 对账确认与正常成功走同一个清稿函数：条件完全相同（requestId、generation、
   * text、draftVersion 全匹配），否则「断线后回来」会清掉一条正常路径不敢清的稿。
   */
  async confirmPending(parentId: string, sideId: string): Promise<void> {
    const view = this.store.read(this.binding.key, sideId);
    const pending = view?.pending;
    if (!view || !pending || pending.state !== "uncertain") return;
    const target: SideTarget = {
      kind: "side",
      hostKey: this.binding.key,
      parentId,
      sideId,
      generation: view.generation,
    };
    await this.reconcile(parentId);
    const fresh = this.store.read(this.binding.key, sideId);
    // 对账期间换了一代：这一份 pending 属于上一代，不能拿来清新旁问。
    if (!fresh || fresh.closed || fresh.generation !== target.generation) return;
    const accepted = await this.acceptedRequestIds(sideId);
    if (accepted) {
      if (accepted.has(pending.requestId)) {
        this.clearSubmittedIfCurrent(sideId, {
          target,
          text: pending.text,
          draftVersion: pending.draftVersion,
          requestId: pending.requestId,
        });
        return;
      }
    }
    // `accepted_requests` 读失败（null）时**不**生成新 id：这一轮只是「还不知道」，
    // 拿同一个 id 再发一次是安全的（服务端按 requestId 幂等），换新 id 不是。
    await this.submit(sideId, pending.text, pending.requestId);
  }

  /** 读一份快照只为拿 `accepted_requests`（第一页就带齐）。 */
  private async acceptedRequestIds(sideId: string): Promise<Set<string> | null> {
    try {
      const page = await this.binding.client.request<SideReadItemsResult>(
        "side/readItems",
        { sideId },
      );
      return new Set(Object.keys(page.accepted_requests ?? {}));
    } catch {
      return null;
    }
  }

  // ----- internals -----

  private generationOf(parentId: string): number | null {
    const views = this.store.list(this.binding.key).filter((v) => v.parentId === parentId);
    return views.length > 0 ? Math.max(...views.map((v) => v.generation)) : null;
  }

  private markUnsynced(parentId: string): void {
    for (const view of this.store.list(this.binding.key)) {
      if (view.parentId !== parentId || view.closed) continue;
      this.store.setSync(this.binding.key, view.sideId, "unsynced");
    }
    this.onChange?.();
  }

  private cacheFrame(n: SideNotification): void {
    const sideId = n.params.side_id;
    let range = this.cache.get(sideId);
    if (!range) {
      range = { from: n.params.seq, frames: [], bytes: 0 };
      this.cache.set(sideId, range);
    }
    range.frames.push(n);
    range.bytes += frameBytes(n);
    if (
      range.frames.length > SIDE_CACHE_MAX_FRAMES ||
      range.bytes > SIDE_CACHE_MAX_BYTES
    ) {
      // 溢出：丢掉这一份缓存并要求全量重读。不能「只留最新的若干帧」——那等于
      // 悄悄接受一个已知有洞的基线。
      this.cache.delete(sideId);
      this.overflowed.add(sideId);
      this.store.setSync(this.binding.key, sideId, "unsynced");
      this.onChange?.();
    }
  }

  /**
   * 把缓存里 `seq > watermark` 的帧按序应用。
   *
   * 缓存按到达顺序存放，而到达顺序就是服务端发布顺序；乱序与重复在这里被
   * `applyOne` 的连续性检查拦下。装基线之后如果缓存里正好接着 baseline，就
   * 一路应用到底；不接（缺号）就丢掉缓存、标未同步、排一次全量对账。
   */
  private replayCached(
    parentId: string,
    sideId: string,
    generation: number,
    watermark: number,
  ): boolean {
    const overflowed = this.overflowed.delete(sideId);
    const range = this.cache.get(sideId);
    this.cache.delete(sideId);
    if (overflowed) {
      this.needsReconcile.add(parentId);
      return false;
    }
    if (!range) return true;
    const view = this.store.read(this.binding.key, sideId);
    if (!view) return true;
    if (view.generation !== generation) return true;

    let seq = watermark;
    for (const frame of range.frames) {
      if (frame.params.generation !== generation) continue;
      if (frame.params.seq <= seq) continue;
      if (frame.params.seq !== seq + 1) {
        // 缓存里本身有洞：全量重读，不跳过去。
        this.needsReconcile.add(parentId);
        return false;
      }
      this.store.apply(this.binding.key, frame);
      seq = frame.params.seq;
    }
    return true;
  }

  private applyOne(n: SideNotification): void {
    const sideId = n.params.side_id;
    const view = this.store.read(this.binding.key, sideId);
    if (!view) {
      // 还没见过这个 side：这是「第一次听说它」。此时**不能**直接把帧写进
      // store —— 那样得到的视图从 seq=1 开始，而真实正文可能已经有几十条，
      // 用户看到一份缺了开头的历史却没有任何提示。正确顺序是先订阅父、再
      // list、再分页装基线（spec §8.2），这一帧进缓存等基线装上后再接。
      //
      // `scheduleFullReconcile` 在第一个 await 之前就占住互斥，所以紧随其后的
      // `cacheFrame` 一定会走缓存分支。
      this.scheduleFullReconcile(n.params.parent_thread_id);
      if (this.reconciling === n.params.parent_thread_id) this.cacheFrame(n);
      return;
    }
    if (view.closed && n.method !== "side/closed") return;
    if (n.params.generation < view.generation) return;
    if (n.params.generation > view.generation) {
      // 新的一代：正文清零由 store 负责，但同步态要重来一次。
      this.store.apply(this.binding.key, n);
      this.onChange?.();
      return;
    }
    if (n.method === "side/closed") {
      // 终态：即便前序缺号也立刻生效，不等一个已不存在的快照。
      this.stopTailPoll();
      this.store.apply(this.binding.key, n);
      this.onChange?.();
      return;
    }
    if (n.params.seq <= view.seq) return;
    if (n.params.seq !== view.seq + 1) {
      this.store.setSync(this.binding.key, sideId, "unsynced");
      this.scheduleFullReconcile(view.parentId);
      this.onChange?.();
      return;
    }
    this.store.apply(this.binding.key, n);
    this.onChange?.();
  }

  /** 缺号后的全量对账。同一父同时只跑一轮，重复请求被 `reconcile` 合并。 */
  private scheduleFullReconcile(parentId: string): void {
    void this.reconcile(parentId).catch(() => {
      // 失败已由 reconcile 内部处理（标记未同步、禁发）。这里吞掉是为了不让
      // 一个 fire-and-forget 的 rejection 冒到全局。
    });
  }

  private startTailPoll(parentId: string): void {
    if (this.tailTimer !== null) return;
    this.tailTimer = setInterval(() => {
      if (this.disposed) return;
      // 可见性已切走：这一轮不必再探（`setVisible(null)` 会停表，这里兜住
      // 「停表与回调交错」的那一帧）。
      if (this.visibleParent !== parentId) return;
      void this.pollTail(parentId);
    }, SIDE_TAIL_POLL_MS);
  }

  private stopTailPoll(): void {
    if (this.tailTimer === null) return;
    clearInterval(this.tailTimer);
    this.tailTimer = null;
  }

  /**
   * 比一次 `last_seq`。有差异才全量对账。
   *
   * 只 list 不比 item 内容：这个探测的目的是发现「丢了最后一帧」，不是拉正文。
   */
  private async pollTail(parentId: string): Promise<void> {
    if (this.polling) return;
    // 刚做完一次全量对账：那一轮已经确认过 last_seq，再 list 一次纯属白读。
    // 用注入的 `clock` 而不是 `Date.now()`，测试才能用 fake timer 精确推进。
    if (
      this.lastReconciledAt !== null &&
      this.clock() - this.lastReconciledAt < SIDE_TAIL_POLL_MS
    ) {
      return;
    }
    this.polling = true;
    try {
      await this.pollTailOnce(parentId);
    } finally {
      this.polling = false;
    }
  }

  private async pollTailOnce(parentId: string): Promise<void> {
    let list: { sides: unknown };
    try {
      list = await this.binding.client.request<{ sides: unknown }>("side/list", {
        parentThreadId: parentId,
      });
    } catch {
      // 网络抖动不算失效：下一轮再看。
      return;
    }
    const sides = parseSideList(list);
    for (const summary of sides) {
      const view = this.store.read(this.binding.key, summary.side_id);
      if (!view) continue;
      if (view.closed) continue;
      if (view.summary.last_seq !== summary.last_seq) {
        this.store.setSync(this.binding.key, summary.side_id, "unsynced");
        this.scheduleFullReconcile(parentId);
        this.onChange?.();
        return;
      }
    }
    if (sides.length === 0 && this.store.list(this.binding.key).some((v) => v.parentId === parentId)) {
      this.scheduleFullReconcile(parentId);
    }
  }

  /**
   * 成功与对账确认共用的清稿。
   *
   * 四个条件缺一不可：换代（generation）、这条提交仍是当前 pending
   * （requestId）、用户没改过文字（text）、也没改回来过（draftVersion）。
   * 少任何一条，一次迟到回包就会清掉用户此刻正在写的内容。
   */
  private clearSubmittedIfCurrent(
    sideId: string,
    snapshot: SideDraftSnapshot & { requestId: string },
  ): void {
    const current = this.store.read(this.binding.key, sideId);
    if (current === undefined) return;
    const matches =
      current.generation === snapshot.target.generation &&
      current.pending?.requestId === snapshot.requestId;
    if (!matches) return;
    this.store.setPending(this.binding.key, sideId, null);
    if (
      current.draftVersion === snapshot.draftVersion &&
      current.draft === snapshot.text
    ) {
      this.store.setDraft(this.binding.key, sideId, "");
    }
    this.onChange?.();
  }

  /**
   * 明确拒绝后的还稿。
   *
   * 原文还在、且用户没写过新东西 → 直接还回输入框，他改一改就能重发。
   * 用户已经写了新稿 → 保留新稿，把这次提交记成 `rejectedSubmission`；悄悄丢掉
   * 会让用户以为那句已经发出去了（spec §5.2）。
   */
  private rejectSubmission(
    sideId: string,
    snapshot: SideDraftSnapshot & { requestId: string },
    message: string,
  ): void {
    const current = this.store.read(this.binding.key, sideId);
    if (!current || current.generation !== snapshot.target.generation) return;
    const untouched =
      current.draftVersion === snapshot.draftVersion &&
      current.draft === snapshot.text;
    if (untouched) {
      this.store.setDraft(this.binding.key, sideId, snapshot.text);
      return;
    }
    current.rejectedSubmission = {
      requestId: snapshot.requestId,
      text: snapshot.text,
      message,
    };
  }
}

export class SideSyncError extends Error {
  constructor(
    readonly reason: string,
    message: string,
  ) {
    super(message);
    this.name = "SideSyncError";
  }
}

/**
 * 网络失败 vs 服务端明确拒绝。
 *
 * 这个区分是整个「不确定提交」逻辑的地基：把服务端拒绝当成网络失败，会让用户
 * 永远卡在「待确认」；反过来则会让一次可能已受理的请求被当成失败重发，同一个
 * 问题被回答两次。
 *
 * 判定依据是错误形状：JSON-RPC 错误带数字 `code`（`data.reason` 在服务端拒绝
 * 里），而传输层失败没有。`unsynced`/`side_not_found` 这类本地错误也算明确失败。
 */
export function isNetworkFailure(error: unknown): boolean {
  if (error instanceof SideSyncError) return false;
  if (typeof error !== "object" || error === null) return true;
  const code = (error as { code?: unknown }).code;
  return typeof code !== "number";
}

/** `side/list` 的行只认形状正确的摘要。 */
function parseSideList(raw: unknown): Array<{ side_id: string; generation: number; last_seq: number }> {
  if (typeof raw !== "object" || raw === null) return [];
  const sides = (raw as { sides?: unknown }).sides;
  if (!Array.isArray(sides)) return [];
  return sides.flatMap((entry) => {
    if (typeof entry !== "object" || entry === null) return [];
    const { side_id, generation, last_seq } = entry as Record<string, unknown>;
    if (typeof side_id !== "string" || side_id.length === 0) return [];
    if (typeof generation !== "number") return [];
    if (typeof last_seq !== "number") return [];
    return [{ side_id, generation, last_seq }];
  });
}
