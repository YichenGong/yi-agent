/**
 * 旁支会话的客户端身份与内存视图（spec §9.1、§8.2）。
 *
 * 这个模块只做三件事，且刻意不碰 `ThreadStore`：
 *
 * 1. **身份**：把「哪条连接、哪个宿主实例」变成一个 key。旁支的 `sideId` 只在
 *    一个宿主进程内有意义——本地和每个远端项目各跑一个 app-server，它们完全
 *    可能都分配出同一个 UUID。用裸 `sideId` 当键，A 项目的旁支会写进 B 项目的
 *    视图，而两边看起来都「合理」。
 * 2. **绑定**：握手结果（能力位 + 随机宿主 id）连到具体的那条连接。宿主重启会
 *    换一个随机 id，于是旧绑定的全部旁支作废——这正是我们要的语义：新进程不
 *    记得任何旁支。
 * 3. **视图**：一个旁支的摘要/正文/用量/待确认提交都放在这里，**不**进普通
 *    会话缓存（IndexedDB、localStorage、会话搜索、操作历史）。旁支是临时的，
 *    它一旦关闭就应当从内存里消失，而不是留下一条可以被「恢复」的记录。
 *
 * 为什么视图要按 `(hostKey, sideId)` 而不是按当前活动会话：手机同时连多条
 * 槽位，每条都可能有旁支。用户切回某个项目时，那里正在跑的旁支必须还在（且
 * 没有被别的项目的同名 sideId 覆盖）。
 */

import type { RpcClient } from "./rpc";
import type { SideReadItemsResult } from "./protocol";
import type { Slot } from "./slots";
import type {
  Item,
  SideEffectiveConfigWire,
  SideStatusWire,
  SideSummaryWire,
  SideUsageWire,
} from "./protocol";
import type { Notification } from "./generated/Notification";
import type { Scope } from "./generated/Scope";

/**
 * 一条连接 × 一个宿主进程实例。
 *
 * 用 `JSON.stringify` 而不是 `slot + ":" + instanceId`：槽位名是用户可控的
 * profile id（可以含冒号），拼接会让 `("a:b", "c")` 与 `("a", "b:c")` 撞成
 * 同一个键。JSON 数组编码没有这个歧义。
 */
export type SideHostKey = string;

/** 旁支的三个权限层（与 Rust 侧 `Scope` 同值）。 */
export type SideScope = Scope;

export interface SideHostBinding {
  key: SideHostKey;
  slot: Slot;
  /** 握手返回的随机宿主 id。旧服务端缺这个字段时为空串，此时 `supported` 恒假。 */
  instanceId: string;
  client: RpcClient;
  /** 该连接上的宿主是否声明了 `side_conversation_v1`。 */
  supported: boolean;
  /**
   * 该连接上的宿主是否声明了 `side_snapshot_lifecycle_v1`。
   *
   * 与 `supported` 分开是刻意的：前者是旧的分页/通知契约（「返回后还能再看到
   * 这条旁支」），后者是本次「退出即销毁 + 票据准入」的契约。新前端必须**两个
   * 都为 true** 才开放入口——只认旧位会把一块已经不可恢复的界面按「可恢复」
   * 的旧语义驱动。
   */
  snapshotLifecycle: boolean;
  scope: SideScope;
  /**
   * 连接当前是否活着。断线时**保留**视图（用户切回来还能看到最后状态），但
   * 一切发送都禁掉：在一条已死的连接上发 `side/turnStart` 只会得到超时。
   */
  connected: boolean;
}

export function sideHostKey(slot: Slot, instanceId: string): SideHostKey {
  return JSON.stringify([slot, instanceId]);
}

/**
 * 从 `initialize` 的回包里解析旁支能力与宿主身份。
 *
 * 两个能力位分别对应两版契约（见 `SideHostBinding.snapshotLifecycle`）。
 *
 * 旧宿主既没有 `capabilities.side_conversation_v1` 也没有 `sideHostInstanceId`。
 * 那种情况**不返回 null**：返回一条 `supported: false` 的绑定，前端据此隐藏
 * 入口、对命令回「不支持」。返回 null 会让调用方不得不造一条「没绑定」的旁路，
 * 而那样一来「缺能力」与「还没握手」就分不开了——前者是终态，后者是中间态。
 *
 * 只有回包根本不是对象（握手异常）才返回 null。
 */
export function parseSideHandshake(
  slot: Slot,
  client: RpcClient,
  result: unknown,
  scope: SideScope,
): SideHostBinding | null {
  if (typeof result !== "object" || result === null) return null;
  const raw = result as {
    capabilities?: unknown;
    sideHostInstanceId?: unknown;
  };
  const caps =
    typeof raw.capabilities === "object" && raw.capabilities !== null
      ? (raw.capabilities as {
          side_conversation_v1?: unknown;
          side_snapshot_lifecycle_v1?: unknown;
        })
      : null;
  const instanceId =
    typeof raw.sideHostInstanceId === "string" ? raw.sideHostInstanceId : "";
  const supported = caps?.side_conversation_v1 === true && instanceId !== "";
  // 没有宿主 id 时两个位都不成立：票据是按 `(hostInstanceId, parent, entryId)`
  // 定位的，缺了宿主身份就无从校验回包。
  const snapshotLifecycle =
    caps?.side_snapshot_lifecycle_v1 === true && instanceId !== "";
  return {
    key: sideHostKey(slot, instanceId),
    slot,
    instanceId,
    client,
    supported,
    snapshotLifecycle,
    scope,
    connected: true,
  };
}

/** 服务端推出的四类旁支帧。命名空间是 `side/`，父 `thread/*` 一帧都不认。 */
export type SideNotification = Extract<
  Notification,
  { method: `side/${string}` }
>;

/**
 * 判别一个通知是否是旁支帧。
 *
 * 不要用 `n.method.startsWith("side/")` 直接当 TS 的类型收窄：`startsWith`
 * 对字符串字面量联合没有收窄能力（`string` 上的判断不会缩小到模板字面量类
 * 型），于是下游仍要 `as`。这里集中做一次真正的判别，调用方拿到的是真类型。
 */
export function isSideNotification(n: Notification): n is SideNotification {
  switch (n.method) {
    case "side/changed":
    case "side/itemUpdated":
    case "side/turnCompleted":
    case "side/closed":
      return true;
    default:
      return false;
  }
}

/**
 * 当前正在看的会话：`main` 是普通会话，`side` 是某个旁支。
 *
 * 父身份（`parentId`）两种都有，因为旁支的所有 UI 操作都需要回到父。旁支额外
 * 带 `generation`：同一个 `sideId` 在宿主上被关闭后重新开放会得到新一代，迟到
 * 的旧帧不得写进新视图。
 */
export type ConversationKey =
  | { kind: "main"; hostKey: SideHostKey; parentId: string }
  | {
      kind: "side";
      hostKey: SideHostKey;
      parentId: string;
      sideId: string;
      generation: number;
    };

/** 一条旁支的**完整**身份。所有跨 `await` 的动作都只认它，不认「当前视图」。 */
export type SideTarget = Extract<ConversationKey, { kind: "side" }>;

/**
 * 提交时捕获的草稿快照。
 *
 * `draftVersion` 是这里唯一能区分「同一段文字」与「用户改走又改回来」的东西：
 * 只比正文的话，两次编辑回到原文看起来就是「没动过」，于是回包会把用户当下的
 * 输入清掉（spec §5.2「身份和草稿修订均匹配才清稿」）。
 */
export interface SideDraftSnapshot {
  target: SideTarget;
  text: string;
  draftVersion: number;
}

/**
 * 一次提交的结果。
 *
 * 三种状态不能压成一个 boolean：`accepted` 可以清稿；`rejected` 要把原文还给
 * 用户并给出原因；`uncertain`（应答在路上丢了）既不能清稿也不能当失败重发——
 * 服务端可能已经受理，重发会让同一个问题被回答两次（spec §5.2、§8.3）。
 */
export type SideSubmitReceipt =
  | { kind: "accepted"; requestId: string }
  | { kind: "uncertain"; requestId: string }
  | { kind: "rejected"; requestId: string; message: string };

/** 一次被明确拒绝、且当时已有更新草稿，因而无处「还回」的提交。 */
export interface RejectedSideSubmission {
  requestId: string;
  text: string;
  message: string;
}

/** 旁支的同步态。`unsynced` 禁发；`synced` 才允许 `side/turnStart`。 */
export type SideSyncState = "unsynced" | "syncing" | "synced";

/**
 * 一次「结果不确定」的提交（spec §8.3）。
 *
 * 断线时请求可能已经被受理、也可能没到服务端。把问题正文留在 `uncertain` 状态
 * 里，重连后先用 `accepted_requests` 对账，或者用**同一个** `requestId` 重试。
 * 绝不能生成一个新 id 或新开一个旁支——那会让同一个问题被回答两次。
 */
export interface PendingSideSubmission {
  requestId: string;
  text: string;
  /** 提交时的草稿版本。清除草稿只在版本与正文都未变时才做。 */
  draftVersion: number;
  state: "submitting" | "uncertain";
}

/** 一个旁支的内存视图。**没有**任何持久化入口，这是刻意的。 */
export interface SideView {
  sideId: string;
  parentId: string;
  generation: number;
  /** 已应用的最高修订序号。基线安装在 Task 15。 */
  seq: number;
  summary: SideSummaryWire;
  /** 线上 `Item` 形状（wire item），不是 runtime 内部的 `SideItem`。 */
  items: Item[];
  draft: string;
  /**
   * 草稿的修订号。只在文字真的变化时递增，且**基线与帧**都不得重置它。
   *
   * 基线替换的是正文（`items`/`usage`），不是用户此刻正在写的东西；把它一起
   * 重置就等于每次对账后所有在途提交的「草稿未变」判定失效。
   */
  draftVersion: number;
  /**
   * 明确被拒、又因为用户已经写了新稿而无处归还的那次提交。
   *
   * 只在内存里留一条，用来告诉用户「你刚才那句没发出去」。绝不悄悄丢掉——那是
   * spec §5.2 里「失败提交保留为独立临时待处理记录」的落点。
   */
  rejectedSubmission: RejectedSideSubmission | null;
  pending: PendingSideSubmission | null;
  usage: SideUsageWire;
  effectiveConfig: SideEffectiveConfigWire | null;
  error: { reason: string | null; message: string } | null;
  /** 滚动锚点（最后可见 item id）。切走再回来要落回原处。 */
  anchor: string | null;
  sync: SideSyncState;
  /** 终态。为真后不再接受任何更新，也不允许复活。 */
  closed: boolean;
}

/** 用量清零的初值：旁支从零累计，不继承父历史的费用。 */
const EMPTY_USAGE: SideUsageWire = {
  total: {
    input: 0,
    output: 0,
    cache_creation: 0,
    cache_read: 0,
    reasoning: 0,
  },
  complete: true,
};

/**
 * 所有连接的旁支视图与绑定。
 *
 * 生命周期契约：
 * - `bind` 同槽位换宿主（`instanceId` 变了）→ 先 `dropHost` 旧的。宿主重启
 *   不复用 sideId，留着旧视图只会让用户点开一个永远 `side_not_found` 的壳。
 * - `setConnected(false)` 保留视图、把同步态打回 `unsynced` 并禁发。
 * - `dropHost` 是唯一的销毁入口（断开远端项目、宿主换代、组件卸载）。
 */
export class SideConversationStore {
  private readonly bindings = new Map<SideHostKey, SideHostBinding>();
  private readonly views = new Map<SideHostKey, Map<string, SideView>>();
  /**
   * 已经被明确退出（销毁）的最高代次，按 `(hostKey, sideId)`。
   *
   * `dropGeneration` 删掉视图之后，旧代**迟到的**帧仍然可能到达（退出时把在途
   * 请求中断了，但已经发出去的帧还在链路上）。没有这份记录，`viewFor` 会按
   * 「第一次听说这个 side」的逻辑重建一个视图——用户刚丢弃的正文就以「一条新的
   * 旁支」的样子回来了，而它其实属于一个已经不存在于宿主上的环境。
   *
   * 只记**代次上界**：同一个 `sideId` 之后可能被重开成更高的一代，那一代必须
   * 被允许建视图，所以判据是 `generation <= retired`，不是「见过这个 sideId」。
   */
  private readonly retired = new Map<SideHostKey, Map<string, number>>();
  /** 退休记录上限：一次会话里退出过多少条旁支就有多少条，按最旧的淘汰。 */
  private static readonly RETIRED_CAP = 512;

  /** 已登记的绑定（只读快照，供 UI 决定是否显示入口）。 */
  binding(hostKey: SideHostKey): SideHostBinding | undefined {
    return this.bindings.get(hostKey);
  }

  bindingsForSlot(slot: Slot): SideHostBinding[] {
    return [...this.bindings.values()].filter((b) => b.slot === slot);
  }

  /**
   * 全部绑定（只读快照）。
   *
   * 供「按 sideId 找归属」这类查询用：`sideId` 只在**一个宿主内**唯一，两个
   * 宿主可以各自有一个 `s1`，所以必须先遍历绑定、再各自查视图，不能反过来。
   */
  allBindings(): SideHostBinding[] {
    return [...this.bindings.values()];
  }

  /**
   * 跨宿主按 `sideId` 查找视图，附带它所属的 `hostKey`。
   *
   * 返回第一个命中即返回：同一个 `sideId` 出现在两个宿主上时，调用方给的
   * 上下文（当前绑定、最近一次操作）才该决定选哪个，故这里不替它猜——调用方
   * 需要精确归属时应直接 `read(hostKey, sideId)`。
   */
  findView(sideId: string): { view: SideView; hostKey: SideHostKey } | undefined {
    for (const binding of this.bindings.values()) {
      const view = this.views.get(binding.key)?.get(sideId);
      if (view) return { view, hostKey: binding.key };
    }
    return undefined;
  }

  bind(binding: SideHostBinding): SideHostBinding {
    for (const existing of [...this.bindings.values()]) {
      if (existing.slot === binding.slot && existing.key !== binding.key) {
        this.dropHost(existing.key);
      }
    }
    this.bindings.set(binding.key, binding);
    return binding;
  }

  /** 连接状态变化。断线保留视图但禁发；重连后仍需一次全量对账才恢复。 */
  setConnected(hostKey: SideHostKey, connected: boolean): void {
    const binding = this.bindings.get(hostKey);
    if (!binding) return;
    binding.connected = connected;
    if (!connected) {
      for (const view of this.views.get(hostKey)?.values() ?? []) {
        if (view.closed) continue;
        view.sync = "unsynced";
        if (view.pending?.state === "submitting") {
          view.pending = { ...view.pending, state: "uncertain" };
        }
      }
    }
  }

  dropHost(hostKey: SideHostKey): void {
    this.bindings.delete(hostKey);
    this.views.delete(hostKey);
    this.retired.delete(hostKey);
  }

  read(hostKey: SideHostKey, sideId: string): SideView | undefined {
    return this.views.get(hostKey)?.get(sideId);
  }

  /**
   * 装入一份分页快照的基线（spec §8.2「最后原子替换本地基线」）。
   *
   * 这是**原子替换**，不是合并：调用方已经按连续 seq 把快照期间缓存的帧收成
   * 一个有序序列，装完之后 `items` 就是那个时刻的全文。合并会把两段不同时刻的
   * 正文拼起来，而快照的整个意义就是「一个确定的时刻」。
   *
   * `page` 里的 metadata（status/usage/active_turn_id/effective_config）只在
   * 第一页有权威值，但每一页都携带同一份，所以这里无条件覆盖——重复写同一份
   * 不可变 metadata 是幂等的。
   */
  installBaseline(
    hostKey: SideHostKey,
    sideId: string,
    page: SideReadItemsResult,
    items: Item[],
  ): void {
    const view = this.viewForBaseline(hostKey, sideId, page);
    view.items = items;
    view.seq = page.snapshot_seq;
    view.summary = {
      ...view.summary,
      status: page.status,
      last_seq: page.snapshot_seq,
      active_turn_id: page.active_turn_id,
    };
    view.usage = page.usage;
    view.effectiveConfig = page.effective_config;
    // 基线只证明「到 snapshot_seq 为止」是一致的。若已经收到过更高的 atom
    // （对账与缓存交错），调用方随后会把那些帧重新应用一遍；这里不把它标成
    // synced，避免在中间态让「发送」短暂放行。
    if (view.seq <= page.snapshot_seq) view.sync = "synced";
  }

  /** 设置同步态（禁发/对账中/已同步）。 */
  /**
   * 记住这条旁支的阅读位置（最后可见 item id）。
   *
   * 只存在内存里：旁支没有持久化入口（见 `SideView` 的注释），删掉重建就该从头
   * 开始。它是**浏览状态**，不该触发任何重渲染——滚动每帧都会写一次。
   */
  setAnchor(hostKey: SideHostKey, sideId: string, anchor: string | null): void {
    const view = this.views.get(hostKey)?.get(sideId);
    if (!view) return;
    view.anchor = anchor;
  }

  setSync(hostKey: SideHostKey, sideId: string, sync: SideSyncState): void {
    const view = this.views.get(hostKey)?.get(sideId);
    if (!view) return;
    view.sync = sync;
  }

  /**
   * 写某条旁支的草稿。草稿按旁支隔离，绝不与父会话共用。
   *
   * 版本只在**文字真的变了**时递增：同一个字重复写入（受控输入框在无关
   * 重渲染时就会这样做）不该让一个已经在途的提交失去「草稿未被改写」的资格。
   * 反过来，改走再改回同一段文字必须算两次变化——否则回包会清掉用户刚打好的
   * 内容（spec §5.2）。
   */
  setDraft(hostKey: SideHostKey, sideId: string, draft: string): void {
    const view = this.views.get(hostKey)?.get(sideId);
    if (!view) return;
    if (view.draft === draft) return;
    view.draft = draft;
    view.draftVersion += 1;
  }

  /**
   * 直接安装草稿正文并把版本推到**至少** `draftVersion`。
   *
   * 只给「自动首问」用：那条问题不是用户敲进来的，但它必须带上一个确定的版本，
   * 好让回包能判断「用户在这期间有没有改过它」。用户在首问安装前已经写了新稿时
   * 调用方不该调这个（见 `SideSyncController.submit` 的 `installQuestion` 分支）。
   */
  installQuestion(
    hostKey: SideHostKey,
    sideId: string,
    text: string,
    draftVersion: number,
  ): void {
    const view = this.views.get(hostKey)?.get(sideId);
    if (!view) return;
    view.draft = text;
    view.draftVersion = Math.max(view.draftVersion, draftVersion);
  }

  /**
   * 清掉某个父上的全部旁支视图。
   *
   * 只在 `side/list` **明确返回空**时调用：那时服务端说「这个父上没有旁支」，
   * 本地留着的必然是幽灵。网络失败时绝不能走这里——那会把用户正在看的旁支
   * 当成已消失。
   */
  clearParent(hostKey: SideHostKey, parentId: string): void {
    const bySide = this.views.get(hostKey);
    if (!bySide) return;
    for (const [sideId, view] of [...bySide]) {
      if (view.parentId === parentId) bySide.delete(sideId);
    }
  }

  /**
   * 只丢弃**精确一代**。
   *
   * 退出旁问时用：这一代已经销毁，它的正文、草稿、用量、分页水位、滚动位置都
   * 不该留在内存里（spec §4.4「不恢复旧问答、旧草稿、旧用量、分页 token、滚动
   * 位置」）。按 `(hostKey, sideId, generation)` 三重匹配删除，于是：
   *
   * - 同一个 `sideId` 在用户返回期间被另一端重开成新一代时，新代**不受影响**；
   * - 旧代一个迟到的帧回来找不到视图，`apply` 本来就是 no-op，不会复活；
   * - 别的父上的旁支一概不动（这是 `clearParent` 做不到的粒度）。
   *
   * 返回是否真的删掉了东西，供调用方决定要不要重渲染。
   */
  dropGeneration(hostKey: SideHostKey, sideId: string, generation: number): boolean {
    const bySide = this.views.get(hostKey);
    if (!bySide) return false;
    const view = bySide.get(sideId);
    if (!view || view.generation !== generation) return false;
    bySide.delete(sideId);
    // 记住这一代已经销毁：它的迟到帧不得重建视图（见 `retired` 的注释）。
    let bySideRetired = this.retired.get(hostKey);
    if (!bySideRetired) {
      bySideRetired = new Map();
      this.retired.set(hostKey, bySideRetired);
    }
    bySideRetired.set(sideId, Math.max(bySideRetired.get(sideId) ?? 0, generation));
    if (bySideRetired.size > SideConversationStore.RETIRED_CAP) {
      // Map 保持插入序：淘汰最早记下的那条。被淘汰只意味着一个非常古老的
      // 迟到帧可能重建视图，而那时它早已不可能再收到新的正文。
      const oldest = bySideRetired.keys().next().value;
      if (oldest !== undefined) bySideRetired.delete(oldest);
    }
    return true;
  }

  /** 设置（或清除）一次未确定提交。 */
  setPending(
    hostKey: SideHostKey,
    sideId: string,
    pending: PendingSideSubmission | null,
  ): void {
    const view = this.views.get(hostKey)?.get(sideId);
    if (!view) return;
    view.pending = pending;
  }

  /** 基线可能先于第一帧到达：没有视图时按快照建一个。 */
  private viewForBaseline(
    hostKey: SideHostKey,
    sideId: string,
    page: SideReadItemsResult,
  ): SideView {
    let bySide = this.views.get(hostKey);
    if (!bySide) {
      bySide = new Map();
      this.views.set(hostKey, bySide);
    }
    const existing = bySide.get(sideId);
    if (existing) return existing;
    const view: SideView = {
      sideId,
      // 快照不带 parent id：调用方知道父，装基线时由 `SideSyncController` 用
      // `view.parentId` 补。这里留空串，`installBaseline` 的调用方负责覆盖。
      parentId: "",
      generation: 0,
      seq: 0,
      summary: {
        side_id: sideId,
        parent_thread_id: "",
        status: page.status,
        snapshot_revision: 0,
        last_seq: page.snapshot_seq,
        active_turn_id: page.active_turn_id,
        expires_at: null,
        generation: 0,
        stopping: false,
      },
      items: [],
      draft: "",
      draftVersion: 0,
      rejectedSubmission: null,
      pending: null,
      usage: page.usage,
      effectiveConfig: page.effective_config,
      error: null,
      anchor: null,
      sync: "unsynced",
      closed: false,
    };
    bySide.set(sideId, view);
    return view;
  }

  /**
   * 让基线视图认领父与代。
   *
   * 分页快照本身不带父/代（它只描述内容），而这两样决定「这帧归谁」。基线装
   * 完之后由对账控制器调用一次，避免在 store 里再复制一份「哪个 side 属于哪个
   * 父」的判定——那正是两处容易写歪的重复逻辑。
   */
  adopt(hostKey: SideHostKey, sideId: string, parentId: string, generation: number): void {
    const view = this.views.get(hostKey)?.get(sideId);
    if (!view) return;
    // 基线安装是**另一条**写入路径（分页 → `installBaseline` → `adopt`），它不
    // 经过 `viewFor` 的退休检查。分页在途时用户退出，迟到的基线会在这里把一代
    // 已经销毁的视图认领下来——所以同一个判据也要在这里执行一次。
    const retiredGeneration = this.retired.get(hostKey)?.get(sideId);
    if (retiredGeneration !== undefined && generation <= retiredGeneration) {
      this.views.get(hostKey)?.delete(sideId);
      return;
    }
    view.parentId = parentId;
    view.generation = generation;
    view.summary = { ...view.summary, parent_thread_id: parentId, generation };
  }

  /** 该宿主上的全部旁支，供「继续旁问」入口与 list 对账使用。 */
  list(hostKey: SideHostKey): SideView[] {
    return [...(this.views.get(hostKey)?.values() ?? [])];
  }

  /**
   * 应用一帧旁支通知。
   *
   * 顺序规则（spec §8.2）：
   * - 旧 generation 的帧**永不**写进新视图；新 generation 重置正文与水位。
   * - `seq <= 已应用` 是重复帧，幂等丢弃（`closed` 除外，见下）。
   * - `side/closed` 是终态：即便前面缺号也立刻生效，不等一个已不存在的快照。
   * - 同一 item 的新版本按 id 替换，**不追加**——否则流式文本会重复。
   *
   * 缺号不在这里处理：Task 15 的对账控制器才会因为 `seq !== seq + 1` 触发全量
   * 重读。store 只保证「不跳洞地乱写」，不替控制器猜。
   */
  apply(hostKey: SideHostKey, n: SideNotification): void {
    const view = this.viewFor(hostKey, n);
    if (!view) return;
    if (this.isStale(view, n)) return;
    // 先按 `n` 本身判别：提前解构 `n.params` 会让 TS 丢掉「method 与 params
    // 形状绑定」的收窄，之后每个字段都要 `as` 一次。
    if (n.method !== "side/closed" && n.params.seq <= view.seq) return;

    switch (n.method) {
      case "side/changed": {
        view.summary = n.params.summary;
        view.seq = n.params.seq;
        break;
      }
      case "side/itemUpdated": {
        upsertItem(view.items, n.params.item);
        view.seq = n.params.seq;
        view.anchor = n.params.item.id;
        break;
      }
      case "side/turnCompleted": {
        const p = n.params;
        view.seq = p.seq;
        view.usage = p.usage;
        view.summary = {
          ...view.summary,
          status: p.status,
          last_seq: p.seq,
          active_turn_id: null,
          stopping: false,
        };
        view.pending = null;
        view.error =
          p.status === "failed"
            ? { reason: p.reason ?? null, message: p.error ?? "" }
            : null;
        break;
      }
      case "side/closed": {
        const p = n.params;
        view.seq = Math.max(view.seq, p.seq);
        view.closed = true;
        view.sync = "synced";
        view.pending = null;
        view.summary = {
          ...view.summary,
          status: "closed" satisfies SideStatusWire,
          stopping: false,
          active_turn_id: null,
        };
        view.error = { reason: p.reason, message: "" };
        break;
      }
    }
  }

  /**
   * 取（必要时新建）该通知所属的视图。
   *
   * 收到的第一帧可以是任意一类：客户端先订阅父再分页，期间的 `itemUpdated`
   * 也不能丢。新建时用一个占位 summary，Task 15 的基线安装会整份替换。
   */
  private viewFor(
    hostKey: SideHostKey,
    n: SideNotification,
  ): SideView | undefined {
    const binding = this.bindings.get(hostKey);
    if (!binding) return undefined;
    let bySide = this.views.get(hostKey);
    if (!bySide) {
      bySide = new Map();
      this.views.set(hostKey, bySide);
    }
    const existing = bySide.get(n.params.side_id);
    if (existing) return existing;
    const retiredGeneration = this.retired.get(hostKey)?.get(n.params.side_id);
    if (retiredGeneration !== undefined && n.params.generation <= retiredGeneration) {
      // 这一代已经被明确销毁：旧帧不重建视图（同上，`retired` 的注释）。
      return undefined;
    }
    if (retiredGeneration !== undefined) {
      // 更高的一代：同一个 `sideId` 被重开了，退休记录不再拦它。
      this.retired.get(hostKey)?.delete(n.params.side_id);
    }
    const view: SideView = {
      sideId: n.params.side_id,
      parentId: n.params.parent_thread_id,
      generation: n.params.generation,
      seq: 0,
      summary: {
        side_id: n.params.side_id,
        parent_thread_id: n.params.parent_thread_id,
        status: "creating",
        snapshot_revision: 0,
        last_seq: 0,
        active_turn_id: null,
        expires_at: null,
        generation: n.params.generation,
        stopping: false,
      },
      items: [],
      draft: "",
      draftVersion: 0,
      rejectedSubmission: null,
      pending: null,
      usage: EMPTY_USAGE,
      effectiveConfig: null,
      error: null,
      anchor: null,
      sync: "unsynced",
      closed: false,
    };
    bySide.set(n.params.side_id, view);
    return view;
  }

  /**
   * 旧 generation 的迟到帧。
   *
   * 两条：比视图新 → 重置正文（新的一代从零开始）；比视图旧 → 丢弃。已关闭的
   * 视图只接受同代的 `closed`（幂等重复关闭），别的都算复活。
   */
  private isStale(view: SideView, n: SideNotification): boolean {
    if (n.params.generation > view.generation) {
      view.generation = n.params.generation;
      view.items = [];
      view.seq = 0;
      view.closed = false;
      view.sync = "unsynced";
      // 换代 = 宿主动给的一条全新旁支。旧的 pending/rejected/草稿版本都属于上一
      // 代的身份，搬进新视图会让一份已经作废的提交去清掉新旁问的稿子。
      view.pending = null;
      view.rejectedSubmission = null;
      view.draft = "";
      view.draftVersion = 0;
      view.error = null;
      view.summary = {
        ...view.summary,
        generation: n.params.generation,
        status: "creating",
        last_seq: 0,
        active_turn_id: null,
        stopping: false,
      };
      return false;
    }
    if (n.params.generation < view.generation) return true;
    if (view.closed && n.method !== "side/closed") return true;
    return false;
  }
}

/** 同一 id 的新版本整条替换（spec：不能追加重复文本）。 */
function upsertItem(items: Item[], item: Item): void {
  const at = items.findIndex((existing) => existing.id === item.id);
  if (at >= 0) items[at] = item;
  else items.push(item);
}

/**
 * 把通知按**来源连接**路由到对应宿主的 store。
 *
 * 这里与 `ownsThreadView` 的分工必须分清：那个函数决定「这条帧该不该写进
 * *当前显示*的会话」，是对**视图**的过滤；旁支的归属是**连接身份**，与当前
 * 显示的是哪条会话无关。先做 `ownsThreadView` 会把非活动槽位的旁支帧整个丢掉，
 * 用户切回那个项目时看到的是一份停住的旧视图。
 *
 * 因此调用方的顺序是：先 `isSideNotification` → 走本类；不是旁支帧才回去做
 * `ownsThreadView` 判定，照旧喂 `ThreadStore`。
 */
export class SideConversationRouter {
  constructor(private readonly store: SideConversationStore) {}

  /** 该 client 在 slot 上的绑定；没有绑定说明还没握手，帧无处可归。 */
  bindingFor(client: RpcClient, slot: Slot): SideHostBinding | null {
    for (const binding of this.store.bindingsForSlot(slot)) {
      if (binding.client === client) return binding;
    }
    return null;
  }

  /** 收到一帧旁支通知。绑定缺失时丢弃——不能退回到「当前连接」去猜。 */
  accept(binding: SideHostBinding | null, n: SideNotification): void {
    if (!binding) return;
    this.store.apply(binding.key, n);
  }

  /** 连接断开：保留视图，标记未同步并禁发。 */
  disconnected(binding: SideHostBinding | null): void {
    if (!binding) return;
    this.store.setConnected(binding.key, false);
  }
}
