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
  info: { cwd: string; model: string } | null;
  /** 服务端权威权限模式；null = 未知,勿当作 normal。 */
  mode: ThreadMode | null;
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
      v.info = { cwd: t.cwd, model: t.model };
    }
  }

  /** 按 `thread_id` 路由一条通知。 */
  applyNotification(n: Notification): void {
    // agent/* notifications belong to a conversation's附属视图 (the subagent
    // rail), not to its transcript, so they are not folded into a Session.
    if (n.method === "agent/children/updated" || n.method === "agent/trace/event") {
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
    if (n.method === "error") {
      // 无 thread 归属的全局错误归当前 thread。
      this.current()?.session.apply(n);
      return;
    }
    const id = n.params.thread_id;
    const v = this.view(id);
    v.session.apply(n);
    if (n.method === "thread/started") v.info = { cwd: n.params.cwd, model: n.params.model };
    if (n.method === "turn/completed" && id !== this.currentId) v.unread = true;
  }

  setApproval(r: ApprovalRequest): void {
    this.view(r.params.thread_id).approval = r;
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
}
