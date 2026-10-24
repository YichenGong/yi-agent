import { Session } from "./session";
import type { ApprovalRequest, Notification, ThreadStatus, ThreadSummary } from "./protocol";
import type { ThreadMode } from "./threadPermissionMode";

/** 单个 thread 的客户端视图：会话 + 服务端权威状态 + 未读 + 待处理审批。 */
export interface ThreadView {
  session: Session;
  status: ThreadStatus;
  /**
   * 是否有未读的回合结束。**派生值**，不是独立状态：
   * `unread = lastCompletedAt > lastReadAt`。
   *
   * 由服务端下发的两个水位算出，故任一端点开会话（服务端推进已读水位并广播
   * `thread/readStateChanged`）都会让其余端的蓝点同时消失。旧实现把它当成
   * 「收到 turn/completed 就置真」的本地布尔位，于是离线跑完的回合永远补不
   * 回来、一端读过另一端仍亮。
   */
  unread: boolean;
  /** 服务端权威的已读水位（epoch 毫秒）；0 = 从未读过。 */
  lastReadAt: number;
  /** 服务端权威的完成水位（epoch 毫秒）；0 = 从未完成过回合。 */
  lastCompletedAt: number;
  approval: ApprovalRequest | null;
  /** 会话身份信息；`model_ref` 为会话的模型引用（null = 跟随全局默认）。 */
  info: { cwd: string; model: string; model_ref: string | null } | null;
  /** 服务端权威权限模式；null = 未知,勿当作 normal。 */
  mode: ThreadMode | null;
  /**
   * 本会话最近一次上报的 reasoning effort：`thread/setReasoningEffort` 回包与
   * `thread/reasoningEffortChanged` 推送给的是**覆盖值**（`null` = 已清除覆盖），
   * `thread/start` / `thread/resume` 回包给的是**生效值**（无覆盖时即条目默认）。
   * 两者在协议上不可区分，故这里如实存下服务端的值，由控件（`ModelPicker`）用清单
   * 条目默认与之比对折回覆盖、决定打勾与「继承 X」标签。纯客户端元数据，不是转录内容。
   */
  reasoningEffort: string | null;
  /**
   * 输入框中尚未发送的草稿。**按 thread 隔离**：切走时留下、切回时原样出现，
   * 否则一处未发出的文字会跟着用户跑到另一个会话里（那不是它要发去的地方）。
   * 纯客户端、纯 UI：不上协议，服务端不知道它的存在。
   */
  draft: string;
  /**
   * Whether this thread's transcript has been reconciled with the server on
   * this connection. `syncing` means the catch-up (resume / readItems) is in
   * flight: the content on screen may be the cached one, so the UI must say so
   * and must not let the user send against it (spec §5).
   */
  syncState: "syncing" | "synced";
}

/**
 * 按 thread 隔离的客户端状态机。通知按 `params.thread_id` 路由到各自的
 * `Session`，因此后台 thread 的流式输出照常累积，切回去即最新。
 *
 * 可变实例：调用方在每次变更后自行触发重渲染（沿用 `App.tsx` 的 `force` 模式）。
 */
export class ThreadStore {
  private views = new Map<string, ThreadView>();
  /**
   * Which source last wrote each thread's `status`: `"live"` for the
   * `thread/status/updated` push stream (authoritative), `"snapshot"` for a
   * `thread/listAll` listing (a point-in-time read that can be stale), or
   * absent when no status has been learned yet. `seed` uses this to avoid
   * rolling a live status back.
   */
  private statusSource = new Map<string, "live" | "snapshot">();
  currentId: string | null = null;

  private create(): ThreadView {
    return {
      session: new Session(),
      status: "idle",
      unread: false,
      lastReadAt: 0,
      lastCompletedAt: 0,
      approval: null,
      info: null,
      mode: null,
      reasoningEffort: null,
      draft: "",
      syncState: "synced",
    };
  }

  /** 取（必要时创建）某 thread 的视图。 */
  view(id: string): ThreadView {
    let v = this.views.get(id);
    if (!v) {
      v = this.create();
      this.views.set(id, v);
    }
    return v;
  }

  /** 只读查询，不创建。 */
  peek(id: string): ThreadView | undefined {
    return this.views.get(id);
  }

  current(): ThreadView | null {
    return this.currentId ? this.view(this.currentId) : null;
  }

  /** 切到某 thread 并立即清除其未读（服务端水位由调用方另行上报）。 */
  select(id: string): void {
    this.currentId = id;
    const v = this.view(id);
    // 本地立刻清点，保留「打开即清」的手感；App 随后发 `thread/markRead`
    // 让服务端水位跟上——否则重连后 `seed` 会按旧水位把蓝点算回来。
    v.unread = false;
    v.lastReadAt = Math.max(v.lastReadAt, v.lastCompletedAt);
  }

  /** 返回当前 thread 的 id 列表（供侧栏派生状态用）。 */
  ids(): string[] {
    return [...this.views.keys()];
  }

  /**
   * Seeds status, cwd/model and the permission mode from a `thread/listAll`
   * snapshot. **Does not touch session content** — sessions are accumulated from
   * notifications only.
   *
   * The snapshot is a point-in-time read that can be stale: the server writes
   * `turn/completed` *before* it persists the turn and flips the thread back to
   * `idle`, and the app re-lists the moment it sees `turn/completed`. A listing
   * issued inside that window still reports `running`. So the snapshot only
   * *seeds* a status the client has never had one for; once the push stream has
   * written one, the snapshot must not roll it back. A default `idle` from a
   * merely-created view is not "written by the push stream" — `statusSource`
   * records which it is.
   *
   * The mode seed is likewise a fill-in-when-missing: a snapshot that does not
   * carry the field (an older server, or a thread it no longer lists) leaves the
   * existing value alone, and can *never* roll a known one back to `null` — that
   * unconditional overwrite was the bug fixed in `8ef49300`. A push
   * (`thread/permissionModeChanged`) and the `thread/resume` / `thread/start`
   * responses stay the higher authority.
   */
  seed(threads: ThreadSummary[]): void {
    for (const t of threads) {
      const v = this.view(t.thread_id);
      if (this.statusSource.get(t.thread_id) !== "live") {
        v.status = t.status ?? "idle";
        this.statusSource.set(t.thread_id, "snapshot");
      }
      v.info = { cwd: t.cwd, model: t.model, model_ref: t.model_ref ?? null };
      // 已读/完成水位按 max 合入：快照是**点读**，可能比推送晚到（一次旧的
      // listAll 回来时水位更小），按 max 合入才不会被回滚——与 `statusSource`
      // 护住 live 状态同一个道理。缺字段（旧服务端）按 0 处理。
      //
      // 这一步是「离线期间跑完的回合」唯一的复活路径：那时没人在听推送，
      // 只有落盘的 `last_completed_at` 记得它跑完了。
      this.mergeWatermarks(t.thread_id, t.last_read_at ?? 0, t.last_completed_at ?? 0);
      // 权限模式同样从快照补：只补「未知」，绝不把已确定的值打回 null（`8ef49300`
      // 修的正是后者）。清单里缺该字段（旧服务端/该会话不在快照中）⇒ 保持未知，
      // 交给 resume 回包或 `thread/permissionModeChanged` 推送定案。
      if (t.permission_mode !== undefined) v.mode = t.permission_mode;
    }
  }

  /**
   * 把服务端下发的两个水位按 `max` 合入视图，并重算 `unread`。
   *
   * 按 `max` 而非直接赋值：已读/完成都是**单调**的事实（时间只会往前走），
   * 而到达顺序不保证——一次晚到的旧快照、一次重放的推送都不该把「已经读过」
   * 打回未读，也不该让「已经跑完」消失。这也让多处来源（listAll / 推送 /
   * markRead 回包）无需区分优先级，各自合入即收敛。
   *
   * 非当前会话才亮蓝点：用户正看着的那条不该有未读角标，这也与 `select()`
   * 的「打开即清」保持一致。
   */
  private mergeWatermarks(id: string, read: number, completed: number): void {
    const v = this.view(id);
    v.lastReadAt = Math.max(v.lastReadAt, read);
    v.lastCompletedAt = Math.max(v.lastCompletedAt, completed);
    v.unread = id !== this.currentId && v.lastCompletedAt > v.lastReadAt;
  }

  /**
   * 按 `thread_id` 路由一条通知。
   *
   * 返回「是否有可见变化」：调用方据此决定要不要重渲染（见 `Session.apply`）。
   * 只有会话内容本身的变化才算——路由到 `views` 之外的元数据（审批框、状态）
   * 由各自的调用点负责刷新。
   */
  applyNotification(n: Notification): boolean {
    // agent/* notifications belong to a conversation's附属视图 (the subagent
    // rail), not to its transcript; `ui/*` is app chrome (theme, git-diff focus,
    // tag-name overrides) handled by App's own notification branch. None is folded
    // into a Session.
    if (
      n.method === "agent/children/updated" ||
      n.method === "agent/trace/event" ||
      n.method === "ui/settings/updated" ||
      n.method === "ui/gitDiff/focus" ||
      // 标签显示名是宿主级偏好，无 thread 归属（`thread_key()` = None），
      // 由 App 的通知分支处理；这条提前返回同时也让下方的 `n.params.thread_id`
      // 收窄仍然成立（TS 否则会因该臂无 `thread_id` 而报错）。
      n.method === "ui/tagNames/updated" ||
      // `item/toolCall/approvalResolved` 是**无 thread 归属**的广播：Rust 侧的载荷
      // 只有 `perm_id` / `by` / `decision`（`protocol.rs` 的
      // `ToolCallApprovalResolved`）。若任其落到下面的 `n.params.thread_id`，类型上
      // 会失败，运行期则会把这条广播错记到当前 thread。它不属于任何 transcript，
      // 显式忽略。
      n.method === "item/toolCall/approvalResolved"
    ) {
      return false;
    }
    if (n.method === "thread/status/updated") {
      const v = this.view(n.params.thread_id);
      v.status = n.params.status;
      this.statusSource.set(n.params.thread_id, "live");
      // 状态推送是比「某条 turn/completed 有没有到」更强的证据：服务端说这条
      // thread 已经不在跑了，那本地的 Stop 就是过期的。只认 `idle`——`running`
      // 与 `awaiting_approval` 都仍是同一轮的延续，Stop 必须保持可用。
      if (n.params.status === "idle") v.session.clearTurn();
      // 离开 awaiting_approval（决定 / 超时 / 中断）即清掉可能残留的审批框，
      // 否则超时后前端会留下一个点不掉的模态。
      if (n.params.status !== "awaiting_approval") v.approval = null;
      return true;
    }
    if (n.method === "thread/modelChanged") {
      // 跨客户端同步通道：另一个客户端（如手机）改了某会话的生效模型，服务端广播
      // 到这里。只更新 `model`——通知带的是解析后的**生效模型**，不含引用名；引用
      // （`model_ref`）由本客户端的下拉写入成功后自行落笔，这里不能替它猜。
      const v = this.view(n.params.thread_id);
      if (v.info) v.info = { ...v.info, model: n.params.model };
      return true;
    }
    if (n.method === "thread/reasoningEffortChanged") {
      // 跨客户端同步通道：另一个客户端切了该会话的 effort，服务端广播到这里。
      // `null`（或缺失）= 已清除覆盖，回落条目默认——按「无覆盖」记录，别当作
      // 「未设置」：控件要显示的是本会话可能正跑着的条目默认档。
      this.view(n.params.thread_id).reasoningEffort = n.params.effort ?? null;
      return true;
    }
    if (n.method === "thread/readStateChanged") {
      // 跨端同步通道：另一个客户端（如手机）点开了这个会话，服务端推进已读
      // 水位后广播到这里。**这就是「手机读过、桌面蓝点同时消失」的唯一入口。**
      // 两个值一起按 max 合入——即便本次只推了已读，完成时间也可能是别的端
      // 后来才看到的，合并后重算才不会漏掉「读完又跑完一轮」的情形。
      this.mergeWatermarks(
        n.params.thread_id,
        n.params.last_read_at,
        n.params.last_completed_at,
      );
      return true;
    }
    if (n.method === "thread/permissionModeChanged") {
      // 跨客户端同步通道：另一个客户端（如桌面）切换了该会话的权限模式，服务端
      // 广播到这里（列表层恒推）。`mode` 是服务端权威值，直接采纳——修复前没有
      // 这条通道，手机上的 chip 只能靠打开会话那一刻的 listAll 快照，桌面之后改
      // 的 YOLO 就永远同步不过来。
      this.view(n.params.thread_id).mode = n.params.mode;
      return true;
    }
    if (n.method === "model/capabilitiesChanged") {
      // 机器级清单属性，不属于任何 thread（App 另有一条总线把它分发给模型相关
      // 组件）。在这里必须显式吞掉：下面那行按 `n.params.thread_id` 取会话，
      // 该变体没有这个字段，落进兜底分支会往 `undefined` 这个假会话里写正文。
      return false;
    }
    if (n.method === "thread/groupsChanged") {
      // 目录级通知：它改的是侧栏的组表，不属于任何会话正文。App 另有分支
      // 重拉 `thread/listAll`，这里只负责别把它漏到下面的兜底分支里。
      return false;
    }
    if (n.method === "error") {
      // 无 thread 归属的全局错误归当前 thread。
      return this.current()?.session.apply(n) ?? false;
    }
    const id = n.params.thread_id;
    const v = this.view(id);
    const visible = v.session.apply(n);
    let changed = visible;
    if (n.method === "thread/started") {
      v.info = { cwd: n.params.cwd, model: n.params.model, model_ref: n.params.model_ref ?? null };
      changed = true;
    }
    if (n.method === "turn/completed") {
      // 广播发生在服务端落盘**之前**，随后的 `thread/listAll` 可能还读到旧的
      // `last_completed_at`。故通知自带 `completed_at`（旧服务端没有：退回本地
      // 时钟），按 max 合入即可收敛，不必另造一套「哪个来源更权威」的机制。
      const completedAt = n.params.completed_at ?? Date.now();
      v.lastCompletedAt = Math.max(v.lastCompletedAt, completedAt);
      // 用户正看着的那个会话不算未读；后台跑完的才算。
      v.unread = id !== this.currentId && v.lastCompletedAt > v.lastReadAt;
      return true;
    }
    return changed;
  }

  setApproval(r: ApprovalRequest): void {
    this.view(r.params.thread_id).approval = r;
  }

  /**
   * 记下某 thread 输入框里未发送的草稿。
   *
   * 每次击键都写：这是「切走时草稿留在原处」的唯一真相来源，而不是在切走那一刻
   * 去猜上一屏的文本（此刻输入框已换成新 thread 的 value）。写入方随后自行触发
   * 重渲染（沿用 App 的 `force` 模式）；文本框本身由视图的 `draft` 驱动，所以
   * 这里不必额外 setState。
   */
  setDraft(id: string, draft: string): void {
    this.view(id).draft = draft;
  }

  clearApproval(threadId: string): void {
    const v = this.views.get(threadId);
    if (v) v.approval = null;
  }

  /** 待确认、且不是当前查看的 thread（供全局横幅）。 */
  pendingApprovalsElsewhere(): ApprovalRequest[] {
    const out: ApprovalRequest[] = [];
    for (const [id, v] of this.views) {
      if (v.approval && id !== this.currentId) out.push(v.approval);
    }
    return out;
  }

  /** 删除某 thread 的全部客户端状态。 */
  drop(id: string): void {
    this.views.delete(id);
    this.statusSource.delete(id);
    if (this.currentId === id) this.currentId = null;
  }

  /**
   * 作废所有「来自推送」的运行状态：宿主进程已死，其推送不再成立。
   *
   * 侧车退出后新进程对任何会话都不再持有内存 agent（全部退化为冷会话）。
   * `statusSource` 却会活过这次重连：`seed()` 不覆盖 `"live"` 的值，于是一条
   * 崩溃前正在跑、`listAll` 现在已报 `idle` 的会话会**一直停在 `running`**。
   * 后果不只是状态徽标骗人——它会让 `selectThread` 误以为该会话仍活着而跳过
   * `thread/resume`（本 card 正是为此不再打断运行中的会话），于是新进程里
   * 那个 agent 永远不会被重建，随后的发送撞上 `-32011 unknown thread` 且无法
   * 自愈（`sendMethodMismatchCode` 只认 `-32013/-32012`）。
   *
   * 清 `statusSource` 而非直接改 `status`：清除后 `seed()` 会按下一份
   * `listAll` 快照重新落值，真实仍活着的会话也会随之从 `running` 复位——因为
   * 新进程会把它们真正重建（resume 或首次发送），此后推送再把状态标回 `"live"`。
   */
  forgetPushStatuses(): void {
    this.statusSource.clear();
  }
}
