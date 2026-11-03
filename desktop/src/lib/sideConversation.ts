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
      ? (raw.capabilities as { side_conversation_v1?: unknown })
      : null;
  const instanceId =
    typeof raw.sideHostInstanceId === "string" ? raw.sideHostInstanceId : "";
  const supported = caps?.side_conversation_v1 === true && instanceId !== "";
  return {
    key: sideHostKey(slot, instanceId),
    slot,
    instanceId,
    client,
    supported,
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

  /** 已登记的绑定（只读快照，供 UI 决定是否显示入口）。 */
  binding(hostKey: SideHostKey): SideHostBinding | undefined {
    return this.bindings.get(hostKey);
  }

  bindingsForSlot(slot: Slot): SideHostBinding[] {
    return [...this.bindings.values()].filter((b) => b.slot === slot);
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
  }

  read(hostKey: SideHostKey, sideId: string): SideView | undefined {
    return this.views.get(hostKey)?.get(sideId);
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
      view.pending = null;
      view.draft = "";
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
