import { Session } from "./session";
import type { ApprovalRequest, Notification, ThreadStatus, ThreadSummary } from "./protocol";
import type { ThreadMode } from "./threadPermissionMode";

/** 单个 thread 的客户端视图：会话 + 服务端权威状态 + 未读 + 待处理审批。 */
export interface ThreadView {
  session: Session;
  status: ThreadStatus;
  /** 未被查看时收到过 turn/completed → true；打开即清除。 */
  unread: boolean;
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
      approval: null,
      info: null,
      mode: null,
      reasoningEffort: null,
      draft: "",
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

  /** 切到某 thread 并清除其未读。 */
  select(id: string): void {
    this.currentId = id;
    this.view(id).unread = false;
  }

  /** 返回当前 thread 的 id 列表（供侧栏派生状态用）。 */
  ids(): string[] {
    return [...this.views.keys()];
  }

  /**
   * Seeds status and cwd/model from a `thread/listAll` snapshot. **Does not
   * touch session content** — sessions are accumulated from notifications only.
   *
   * The snapshot is a point-in-time read that can be stale: the server writes
   * `turn/completed` *before* it persists the turn and flips the thread back to
   * `idle`, and the app re-lists the moment it sees `turn/completed`. A listing
   * issued inside that window still reports `running`. So the snapshot only
   * *seeds* a status the client has never had one for; once the push stream has
   * written one, the snapshot must not roll it back. A default `idle` from a
   * merely-created view is not "written by the push stream" — `statusSource`
   * records which it is.
   */
  seed(threads: ThreadSummary[]): void {
    for (const t of threads) {
      const v = this.view(t.thread_id);
      if (this.statusSource.get(t.thread_id) !== "live") {
        v.status = t.status ?? "idle";
        this.statusSource.set(t.thread_id, "snapshot");
      }
      v.info = { cwd: t.cwd, model: t.model, model_ref: t.model_ref ?? null };
    }
  }

  /** 按 `thread_id` 路由一条通知。 */
  applyNotification(n: Notification): void {
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
      return;
    }
    if (n.method === "thread/status/updated") {
      const v = this.view(n.params.thread_id);
      v.status = n.params.status;
      this.statusSource.set(n.params.thread_id, "live");
      // 离开 awaiting_approval（决定 / 超时 / 中断）即清掉可能残留的审批框，
      // 否则超时后前端会留下一个点不掉的模态。
      if (n.params.status !== "awaiting_approval") v.approval = null;
      return;
    }
    if (n.method === "thread/modelChanged") {
      // 跨客户端同步通道：另一个客户端（如手机）改了某会话的生效模型，服务端广播
      // 到这里。只更新 `model`——通知带的是解析后的**生效模型**，不含引用名；引用
      // （`model_ref`）由本客户端的下拉写入成功后自行落笔，这里不能替它猜。
      const v = this.view(n.params.thread_id);
      if (v.info) v.info = { ...v.info, model: n.params.model };
      return;
    }
    if (n.method === "thread/reasoningEffortChanged") {
      // 跨客户端同步通道：另一个客户端切了该会话的 effort，服务端广播到这里。
      // `null`（或缺失）= 已清除覆盖，回落条目默认——按「无覆盖」记录，别当作
      // 「未设置」：控件要显示的是本会话可能正跑着的条目默认档。
      this.view(n.params.thread_id).reasoningEffort = n.params.effort ?? null;
      return;
    }
    if (n.method === "thread/permissionModeChanged") {
      // 跨客户端同步通道：另一个客户端（如桌面）切换了该会话的权限模式，服务端
      // 广播到这里（列表层恒推）。`mode` 是服务端权威值，直接采纳——修复前没有
      // 这条通道，手机上的 chip 只能靠打开会话那一刻的 listAll 快照，桌面之后改
      // 的 YOLO 就永远同步不过来。
      this.view(n.params.thread_id).mode = n.params.mode;
      return;
    }
    if (n.method === "error") {
      // 无 thread 归属的全局错误归当前 thread。
      this.current()?.session.apply(n);
      return;
    }
    const id = n.params.thread_id;
    const v = this.view(id);
    v.session.apply(n);
    if (n.method === "thread/started")
      v.info = { cwd: n.params.cwd, model: n.params.model, model_ref: n.params.model_ref ?? null };
    if (n.method === "turn/completed" && id !== this.currentId) v.unread = true;
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
