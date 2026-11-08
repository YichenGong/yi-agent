/**
 * 转录滚动的**唯一**位置决策者（Spec §4.4 第 5 条）。
 *
 * 为什么必须收敛到一个对象：旧 `ChatView` 同时有五个地方写 `scrollTop`——
 * `useStickToBottom` 的贴底、整体高度差补偿 effect、锚点恢复、锚点漂移校正、
 * 窗口化后的 `requestFocus`。它们在真实浏览器里互相覆盖，表现为用户被拽回底部、
 * 图片加载后视口跳走、切换会话后位置错乱。同一帧只允许一个决策，才不会打架。
 *
 * 三条规则，优先级从高到低：
 * 1. **pending 恢复**（入场锚点 / 搜索跳转）：只写一次，可容纳之前保持 pending；
 * 2. **阅读锚点**：按锚点行的真实坐标纠正，而不是把 `scrollHeight` 差当成上方新增；
 * 3. **贴底**：仅当仍然 following。
 *
 * 关键判据：`acknowledgeWrite` 让浏览器 clamp 后的实际值回流，但**不改变意图**。
 * 程序化写到 `maxTop` 不等于用户要贴底——那是布局结果。
 *
 * 纯状态机：零 DOM、零时间。调用方负责读写元素并把几何喂进来。
 */

import type { ViewAnchor } from "./viewAnchor";

/** 一次布局提交后的可用几何。 */
export interface ScrollLayout {
  /** `(slot/host, threadId, contentEpoch, layoutSignature)` 拼出的身份。 */
  owner: string;
  /** 内容代次：回放/快照安装换 generation，几何不再可比。 */
  generation: number;
  /** 滚动上限 `scrollHeight - clientHeight`。 */
  maxTop: number;
  /** 当前 `scrollTop`。 */
  scrollTop: number;
  /** 视口顶那条消息；`null` = 空白占位区里没有可视消息。 */
  anchor: ViewAnchor | null;
  /** 该锚消息顶边在滚动内容坐标里的位置。 */
  anchorTop: number | null;
  /**
   * **阅读锚点**那条消息顶边在内容坐标里的位置（`readingAnchor` 已挂载时才有）。
   *
   * 必须与 `anchorTop` 分开：`anchor` 是「视口顶部那条」，恢复之后它往往不是
   * 持有阅读偏移的那条。拿 `anchorTop` 去减保存的偏移，减的是**另一行**的坐标，
   * 于是 delta 恒为 0、纠正永不生效——上方内容长高时视口就整体位移（实测恢复
   * 1312 却停在 1012，缺的正是锚点行真实的 1312）。
   */
  readingAnchorTop?: number | null;
  /**
   * 参与「位移纠正」的那些消息**当前**的内容坐标。
   *
   * 由调用方查 DOM 填进来。协调器只关心「这些 id 现在在哪」，不关心怎么量的——
   * 这样它仍是纯状态机，而「上一份几何 vs 这一份几何」的比较也不需要它做 DOM。
   */
  positions?: Record<string, number> | null;
  /**
   * 需要调用方提供**当前**坐标的消息 id（去重，至多两条）：pending 目标 +
   * 本次纠正的参照消息（持有的阅读锚点，否则视口顶那条）。
   *
   * 参照消息之所以要由调用方按 id 去查，而不是直接用 `anchor`：布局变化后
   * 「视口顶那条」可能已经换了另一条，不能拿新那条比位移。
   */
  trackedAnchorIds?: string[] | null;
  /**
   * pending 恢复目标行的几何（已挂载时才有）。
   *
   * 必须与 `anchor` 分开：目标行可能还没挂载，或者视口顶其实落在别的一行上。
   * 用「当前可视行」当目标会把视口写到错误位置——这是锚点恢复错位最常见的成因。
   */
  target?: { itemId: string; top: number } | null;
}

export type PositionPlan =
  | { kind: "none" }
  | { kind: "write"; top: number; following: boolean };

/** 亚像素与行内重排不算位移，避免无意义地反复写 `scrollTop`。 */
const WRITE_EPSILON_PX = 1;

const clamp = (v: number, lo: number, hi: number): number => Math.max(lo, Math.min(hi, v));

export class TranscriptScrollCoordinator {
  readonly owner: string;
  private followingIntent = true;
  private pendingRestore: ViewAnchor | null = null;
  /** 最近一次确认有效的阅读锚点。空白占位区不会用 null 覆盖它。 */
  private readingAnchor: ViewAnchor | null = null;
  private lastWrite: number | null = null;

  /** 最近一次布局几何（诊断与「每次布局维护上一份」的落点）。 */
  private lastLayout: ScrollLayout | null = null;

  constructor(owner: string) {
    this.owner = owner;
  }

  isFollowing(): boolean {
    return this.followingIntent;
  }

  /**
   * 用户真实滚动。
   *
   * 只有**用户**把视口拖到底部才恢复跟随；程序化写入走 `acknowledgeWrite`，
   * 那条路径永远不会改变意图。
   */
  acceptUserScroll(top: number, maxTop: number): void {
    // 用户一旦自己滚动，未完成的恢复就作废：再写一次会把人拽走。
    this.pendingRestore = null;
    const atBottom = maxTop - top <= 1;
    if (atBottom) this.followingIntent = true;
    else this.followingIntent = false;
    // 用户接管位置后，之前记住的锚点不再有效：继续按它纠正就是把用户拽回去。
    this.readingAnchor = null;
  }

  /** 放弃跟随但不动视口（恢复阅读位置后必须调用，否则下一个 delta 会拽回底部）。 */
  releaseFollow(): void {
    this.followingIntent = false;
  }

  /** 显式「回到最新」：发消息、点回到底部、切换会话。 */
  requestLatest(): void {
    this.followingIntent = true;
    this.pendingRestore = null;
    this.readingAnchor = null;
  }

  /** 尚未完成的恢复目标 id（调用方据此在 DOM 里找它的真实位置）。 */
  pendingRestoreId(): string | null {
    return this.pendingRestore?.itemId ?? null;
  }

  /** 当前持有的阅读锚点 id（调用方据此在 DOM 里找它的真实位置）。 */
  readingAnchorId(): string | null {
    return this.readingAnchor?.itemId ?? null;
  }

  /**
   * 调用方需要为哪些消息提供**当前**内容坐标。
   *
   * 必须在 `planLayout` **之前**调用：决策要用到这次布局的坐标，而记账在决策后。
   */
  trackedAnchorIds(): string[] {
    const ids: string[] = [];
    if (this.pendingRestore !== null) ids.push(this.pendingRestore.itemId);
    const ref = this.readingAnchor?.itemId ?? this.lastLayout?.anchor?.itemId ?? null;
    if (ref !== null && !ids.includes(ref)) ids.push(ref);
    return ids;
  }

  /** 是否还有未完成的恢复。`false` 表示最近一次 `requestRestore` 已落地。 */
  hasPendingRestore(): boolean {
    return this.pendingRestore !== null;
  }

  /** 请求把某个锚点恢复到它当初的视口偏移。 */
  requestRestore(anchor: ViewAnchor): void {
    this.pendingRestore = anchor;
  }

  /**
   * 记录一次布局里的几何，供**下一次**布局比较（每次布局提交都要维护上一份）。
   *
   * **不能**顺手把 `readingAnchor` 换成 `layout.anchor`：那会把「当初保存的视口
   * 偏移」改写成「此刻的偏移」，于是 `desired = anchorTop - offset` 恒等于当前
   * `scrollTop`，纠正永远不会发生——锚点上方增高就再也对不回去。
   * 持有的锚点只由 `requestRestore` / `holdAnchor` 写。
   */
  acceptLayout(layout: ScrollLayout): void {
    if (layout.owner !== this.owner) return;
    this.lastLayout = layout;
  }

  /**
   * 记住一条阅读锚点（用户当前读的那条 + 它此刻的视口偏移）。
   *
   * 用于用户自己滚动后重新采集：此后锚点上方的高变化必须纠正回来。
   */
  holdAnchor(anchor: ViewAnchor): void {
    this.readingAnchor = anchor;
  }

  /** 决定这一次布局要不要写位置、写到哪里。 */
  planLayout(layout: ScrollLayout): PositionPlan {
    if (layout.owner !== this.owner) return { kind: "none" };
    const maxTop = Math.max(0, layout.maxTop);

    // 1) pending 恢复：可容纳才写，否则保持 pending 等更多内容。
    if (this.pendingRestore !== null) {
      const target = layout.target ?? null;
      if (target !== null && target.itemId === this.pendingRestore.itemId) {
        const desired = target.top - this.pendingRestore.offset;
        if (desired >= 0 && desired <= maxTop) {
          this.readingAnchor = this.pendingRestore;
          this.pendingRestore = null;
          return this.write(clamp(desired, 0, maxTop), false);
        }
      }
      // 目标尚未挂载或内容不足以容纳：不写、不标已恢复。
      return { kind: "none" };
    }

    // 2) 阅读锚点：按**该消息自己的实际坐标**恢复（Spec §4.4 第 3、4 条）。
    //
    // 保留的是「消息 id + 它当初相对视口顶的偏移」；布局变化后重新量这条消息的
    // 坐标，`坐标 - 偏移` 就是视口该在的位置。于是：
    //   - 内容长在锚点**下方** -> 坐标不变 -> desired == 当前，不写；
    //   - 内容长在锚点**上方** -> 坐标变大 -> 视口跟着下移同样的量；
    //   - 整体 `scrollHeight` 差从来没有参与计算，不会被当成上方新增。
    //
    // 坐标优先取 `positions[锚点 id]`（这条消息自己的位置）。**不能**用
    // `anchorTop`：那是「当前视口顶那条」的位置，布局变化后往往是另一条消息，
    // 拿它减保存的偏移会算出完全错误的落点（实测把 5300 算成 10300）。
    if (!this.followingIntent) {
      const held = this.readingAnchor ?? this.lastLayout?.anchor ?? null;
      if (held !== null) {
        // 回退顺序：这条消息自己的坐标 -> 阅读锚点坐标 -> 视口顶那条的坐标。
        // 后两者只在「视口顶正好是持有的那一条」时等价，所以排在自己坐标之后；
        // 但它们也是刚恢复/刚提交布局那一刻唯一可用的信息。
        const heldTop =
          layout.positions?.[held.itemId] ??
          layout.readingAnchorTop ??
          (this.readingAnchor !== null && this.readingAnchor.itemId === layout.anchor?.itemId
            ? layout.anchorTop
            : null);
        if (heldTop !== null && heldTop !== undefined) {
          const desired = heldTop - held.offset;
          if (
            Math.abs(desired - layout.scrollTop) >= WRITE_EPSILON_PX &&
            desired <= maxTop
          ) {
            return this.write(clamp(desired, 0, maxTop), false);
          }
        }
      }
    }

    // 3) 仍在跟随：贴底。
    if (this.followingIntent) {
      const desired = maxTop;
      if (Math.abs(desired - layout.scrollTop) < WRITE_EPSILON_PX) return { kind: "none" };
      return this.write(desired, true);
    }

    return { kind: "none" };
  }

  /**
   * 写入完成（浏览器可能已 clamp）。
   *
   * 这里**只记录**实际值，不改意图：程序化写到 `maxTop` 是布局结果，
   * 不能读成「用户要贴底」。
   */
  acknowledgeWrite(actualTop: number, _maxTop: number): void {
    this.lastWrite = actualTop;
  }

  /** 上一次写入的实际位置（诊断用）。 */
  lastWrittenTop(): number | null {
    return this.lastWrite;
  }

  /** 最近一次布局几何（诊断用）。 */
  lastGeometry(): ScrollLayout | null {
    return this.lastLayout;
  }

  private write(top: number, following: boolean): PositionPlan {
    // 写位置与意图一起返回：调用方在同一次布局里只允许执行这一个写入。
    this.followingIntent = following;
    this.lastWrite = top;
    return { kind: "write", top, following };
  }
}
