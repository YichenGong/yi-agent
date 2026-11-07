/**
 * Per-task draft and action bookkeeping for the subagent trace panel.
 *
 * Three things make this more than a form model:
 *
 * 1. **A reply belongs to the record that sent it, not to the key.** Revisit a
 *    task after a delete/rebind and the key is the same while the record is a new
 *    one, so a late reply must settle the old operation and never write into the
 *    new draft. Every record carries its own lifetime number and every operation
 *    its own id.
 * 2. **Cancelling is two-phase.** The preview hands out a token that is bound to
 *    the client, connection generation and a *preview epoch*; navigating away,
 *    switching tabs, a task turning terminal or a new connection all bump that
 *    epoch, so a token can never be confirmed against a view it was not issued
 *    for.
 * 3. **Waiting is a hint, not a timeout.** Fifteen seconds only publishes
 *    "outcome unknown"; the real request keeps ownership of its lock until the
 *    transport settles, exactly like the subscription module.
 *
 * No React here: the panel subscribes through `subscribe`/`read`.
 */
import { ConnectionError } from "./connectionError";
import { formatError } from "./errorMessage";
import type { AgentCancelPreviewResult } from "./protocol";
import type { SubagentRow } from "./subagents";
import { cancelDeadline } from "./subagentTraceModel";
import {
  type TraceConnection,
  type TraceTarget,
  traceTargetKey,
} from "./subagentTraceSubscription";

export type TraceOperation = "message" | "preview" | "confirm";

/** What the confirmation UI may show: no client, no epoch, no target. */
export interface CancelConfirmation {
  token: string;
  taskIds: readonly string[];
  deadline: number;
}

export interface TraceActionSnapshot {
  draft: string;
  draftRevision: number;
  messagePending: boolean;
  previewPending: boolean;
  confirmPending: boolean;
  confirmation: CancelConfirmation | null;
  error: string | null;
  notice: string | null;
}

const MESSAGE = "agent/message";
const PREVIEW = "agent/cancel/preview";
const CANCEL = "agent/cancel";

/** Same wait as the subscription's hint: one number for the whole panel. */
const ACTION_WAIT_HINT_MS = 15_000;

const NO_CONNECTION = "not connected";

const BLANK_TEXT = "消息不能为空";
const MOTD_QUEUED = "消息已入队";
const MOTD_CANCELLED = "取消请求已提交";
const NOTHING_TO_CANCEL = "该任务当前没有可取消的子任务。";
const CONFIRMATION_EXPIRED = "取消确认已失效，请重新预览后再确认。";
const CANCEL_FAILED = "取消失败，请重新预览后再试。";
const WAITING_MESSAGE = "仍在等待服务端确认，结果尚未返回。";
const WAITING_PREVIEW = "预览请求尚未返回，结果尚未确定。";
const WAITING_CONFIRM = "取消请求尚未返回，结果尚未确定。";

/** The public face of a preview the actions still own. */
interface PendingConfirmation {
  token: string;
  taskIds: readonly string[];
  deadline: number;
  /** The exact client and generation the token was issued against. */
  client: TraceConnection["client"];
  connectionGeneration: number;
  targetKey: string;
  previewEpoch: number;
}

interface TaskRecord {
  /** The identity this record was created for; drop helpers match on it. */
  target: TraceTarget;
  /** Bumped every time a record is created; late replies compare it. */
  lifetime: number;
  draft: string;
  draftRevision: number;

  messagePending: boolean;
  messageOperation: number;
  messageTimedOut: boolean;
  messageTimer: ReturnType<typeof setTimeout> | null;

  previewPending: boolean;
  previewOperation: number;
  previewTimedOut: boolean;
  previewTimer: ReturnType<typeof setTimeout> | null;

  confirmPending: boolean;
  confirmOperation: number;
  confirmTimedOut: boolean;
  confirmTimer: ReturnType<typeof setTimeout> | null;

  confirmation: PendingConfirmation | null;
  confirmationTimer: ReturnType<typeof setTimeout> | null;
  /**
   * Invalidates every token and in-flight cancel reply for this record. Bumped
   * on leaving the record, on a terminal state, on disconnect and on drop.
   */
  previewEpoch: number;

  /** The last known terminal flag, so a late message cannot clear a dead draft. */
  finished: boolean;

  error: string | null;
  notice: string | null;

  snapshot: TraceActionSnapshot;
}

/** A record that has never been written to still reads as a stable object. */
const EMPTY_SNAPSHOT: TraceActionSnapshot = Object.freeze({
  draft: "",
  draftRevision: 0,
  messagePending: false,
  previewPending: false,
  confirmPending: false,
  confirmation: null,
  error: null,
  notice: null,
});

function sameIds(a: readonly string[], b: readonly string[]): boolean {
  return a.length === b.length && a.every((value, index) => value === b[index]);
}

function sameTarget(target: TraceTarget, key: string): boolean {
  return traceTargetKey(target) === key;
}

function dedupe(ids: readonly string[]): string[] {
  const seen = new Set<string>();
  const unique: string[] = [];
  for (const id of ids) {
    if (seen.has(id)) continue;
    seen.add(id);
    unique.push(id);
  }
  return unique;
}

export class TraceActions {
  private readonly records = new Map<string, TaskRecord>();
  private readonly listeners = new Set<() => void>();
  private visibleKey: string | null = null;
  private lifetime = 0;
  private operation = 0;
  private now: () => number = () => performance.now();

  readonly subscribe = (listener: () => void): () => void => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };

  /** The record for one task, created lazily by the first write. */
  private ensure(target: TraceTarget): TaskRecord {
    const key = traceTargetKey(target);
    const existing = this.records.get(key);
    if (existing) return existing;

    const record: TaskRecord = {
      target: { ...target },
      lifetime: ++this.lifetime,
      draft: "",
      draftRevision: 0,
      messagePending: false,
      messageOperation: 0,
      messageTimedOut: false,
      messageTimer: null,
      previewPending: false,
      previewOperation: 0,
      previewTimedOut: false,
      previewTimer: null,
      confirmPending: false,
      confirmOperation: 0,
      confirmTimedOut: false,
      confirmTimer: null,
      confirmation: null,
      confirmationTimer: null,
      previewEpoch: 0,
      finished: false,
      error: null,
      notice: null,
      snapshot: EMPTY_SNAPSHOT,
    };
    this.records.set(key, record);
    return record;
  }

  read(target: TraceTarget): TraceActionSnapshot {
    return this.records.get(traceTargetKey(target))?.snapshot ?? EMPTY_SNAPSHOT;
  }

  // ---------------------------------------------------------------------------
  // Drafts and selection
  // ---------------------------------------------------------------------------

  /** Enter or leave the panel. Leaving invalidates tokens but keeps drafts. */
  setVisible(target: TraceTarget | null): void {
    const nextKey = target === null ? null : traceTargetKey(target);
    if (nextKey === this.visibleKey) return;

    const previous = this.visibleKey === null ? null : this.records.get(this.visibleKey);
    this.visibleKey = nextKey;
    if (previous) {
      // The token was issued for a view the user has left, even if they come
      // straight back to the same record.
      previous.previewEpoch += 1;
      previous.confirmation = null;
      this.clearConfirmationTimer(previous);
      this.publish(previous);
    }
    if (target !== null) this.ensure(target);
  }

  setDraft(target: TraceTarget, text: string): void {
    const record = this.ensure(target);
    if (record.draft === text) return;
    record.draft = text;
    record.draftRevision += 1;
    this.publish(record);
  }

  /**
   * The rail says this task is now terminal (or no longer is).
   *
   * A finished task cannot be cancelled, so its token dies with the transition.
   * The draft is deliberately untouched: the user may still be typing.
   */
  updateTask(target: TraceTarget, row: SubagentRow | null): void {
    if (row === null) return;
    const record = this.records.get(traceTargetKey(target));
    if (!record) return;

    const wasFinished = record.finished;
    record.finished = row.finished;
    if (!row.finished) return;

    const invalidate = !wasFinished || record.confirmation !== null || record.previewPending
      || record.confirmPending;
    if (!invalidate) return;
    record.previewEpoch += 1;
    record.confirmation = null;
    this.clearConfirmationTimer(record);
    this.publish(record);
  }

  /** Drop a settled confirmation. A pending confirm cannot be taken back. */
  dismiss(target: TraceTarget): void {
    const record = this.records.get(traceTargetKey(target));
    if (!record || record.confirmPending || record.confirmation === null) return;
    record.confirmation = null;
    this.clearConfirmationTimer(record);
    this.publish(record);
  }

  // ---------------------------------------------------------------------------
  // Message
  // ---------------------------------------------------------------------------

  async send(target: TraceTarget, connection: TraceConnection | null): Promise<void> {
    const record = this.ensure(target);
    if (connection === null || !this.matches(target, connection)) {
      record.error = formatError(new ConnectionError("disconnected", NO_CONNECTION));
      this.publish(record);
      throw new ConnectionError("disconnected", NO_CONNECTION);
    }
    if (record.messagePending) return;
    if (record.draft.trim().length === 0) {
      record.error = BLANK_TEXT;
      this.publish(record);
      return;
    }

    const operation = ++this.operation;
    const revision = record.draftRevision;
    const text = record.draft;
    record.messagePending = true;
    record.messageOperation = operation;
    record.messageTimedOut = false;
    record.error = null;
    record.notice = null;
    this.clearMessageTimer(record);
    record.messageTimer = setTimeout(() => {
      record.messageTimer = null;
      // The hint never releases the lock: only the transport may do that.
      if (record.messageOperation !== operation || !record.messagePending) return;
      record.messageTimedOut = true;
      record.notice = WAITING_MESSAGE;
      this.publish(record);
    }, ACTION_WAIT_HINT_MS);
    this.publish(record);

    try {
      await connection.client.request(MESSAGE, {
        threadId: target.threadId,
        taskId: target.taskId,
        message: text,
      });
    } catch (error) {
      this.settleMessage(record, operation, () => {
        // The hint is replaced by the truth, whichever way the request went.
        record.notice = null;
        record.error = formatError(error);
      });
      throw error;
    }

    this.settleMessage(record, operation, (timedOut) => {
      // The draft is cleared only while it still holds exactly what was sent, the
      // task can still take it, and nobody has been told the outcome is unknown.
      if (!timedOut && record.draftRevision === revision && !record.finished) record.draft = "";
      record.notice = MOTD_QUEUED;
    });
  }

  /**
   * Unlock the message operation and (only for the newest one) write its outcome.
   *
   * A dropped record is not recreated, but the caller's promise still settles:
   * the reply belongs to the operation, not to the row that happened to exist.
   */
  private settleMessage(
    record: TaskRecord,
    operation: number,
    outcome: (timedOut: boolean) => void,
  ): void {
    if (record.messageOperation !== operation) return;
    record.messagePending = false;
    this.clearMessageTimer(record);
    const timedOut = record.messageTimedOut;
    record.messageTimedOut = false;
    outcome(timedOut);
    this.publish(record);
  }

  // ---------------------------------------------------------------------------
  // Cancel: preview
  // ---------------------------------------------------------------------------

  async preview(target: TraceTarget, connection: TraceConnection | null): Promise<void> {
    const record = this.ensure(target);
    if (connection === null || !this.matches(target, connection)) {
      record.error = formatError(new ConnectionError("disconnected", NO_CONNECTION));
      this.publish(record);
      throw new ConnectionError("disconnected", NO_CONNECTION);
    }
    if (record.previewPending) return;

    const operation = ++this.operation;
    const epoch = record.previewEpoch;
    const startedAt = this.now();
    record.previewPending = true;
    record.previewOperation = operation;
    record.previewTimedOut = false;
    record.error = null;
    this.clearPreviewTimer(record);
    record.previewTimer = setTimeout(() => {
      record.previewTimer = null;
      if (record.previewOperation !== operation || !record.previewPending) return;
      record.previewTimedOut = true;
      record.notice = WAITING_PREVIEW;
      this.publish(record);
    }, ACTION_WAIT_HINT_MS);
    this.publish(record);

    let result: AgentCancelPreviewResult;
    try {
      result = await connection.client.request<AgentCancelPreviewResult>(PREVIEW, {
        threadId: target.threadId,
        taskId: target.taskId,
      });
    } catch (error) {
      this.settlePreview(record, operation, () => {
        record.error = formatError(error);
      });
      throw error;
    }

    this.settlePreview(record, operation, () => {
      // A preview that lands after the user left, after the task finished, or
      // after the 15s hint is stale by definition: its token must not survive.
      const stale = record.previewEpoch !== epoch || record.finished || record.previewTimedOut;
      if (stale) return;

      const taskIds = dedupe(result.taskIds);
      const deadline = cancelDeadline(startedAt, result.expiresInSecs);
      if (deadline === null || deadline <= this.now()) return;

      record.confirmation = {
        token: result.confirmationToken,
        taskIds,
        deadline,
        client: connection.client,
        connectionGeneration: connection.connectionGeneration,
        targetKey: traceTargetKey(target),
        previewEpoch: epoch,
      };
      this.clearConfirmationTimer(record);
      record.confirmationTimer = setTimeout(() => {
        record.confirmationTimer = null;
        if (record.confirmation?.token !== result.confirmationToken) return;
        record.confirmation = null;
        record.previewEpoch += 1;
        this.publish(record);
      }, deadline - this.now());
    });
  }

  private settlePreview(record: TaskRecord, operation: number, outcome: () => void): void {
    if (record.previewOperation !== operation) return;
    record.previewPending = false;
    this.clearPreviewTimer(record);
    outcome();
    this.publish(record);
  }

  // ---------------------------------------------------------------------------
  // Cancel: confirm
  // ---------------------------------------------------------------------------

  async confirm(target: TraceTarget, connection: TraceConnection | null): Promise<void> {
    const record = this.records.get(traceTargetKey(target));
    if (!record) return;

    const confirmation = record.confirmation;
    if (confirmation === null) {
      record.error = CONFIRMATION_EXPIRED;
      this.publish(record);
      return;
    }
    if (record.confirmPending) return;

    // The token is bound to the client and generation that issued it. A new
    // connection must never borrow it, and a changed view must not confirm.
    const borrowed = connection === null
      || connection.client !== confirmation.client
      || connection.connectionGeneration !== confirmation.connectionGeneration
      || !this.matches(target, connection);
    if (borrowed
      || confirmation.targetKey !== traceTargetKey(target)
      || confirmation.previewEpoch !== record.previewEpoch
      || !sameTarget(target, confirmation.targetKey)) {
      record.confirmation = null;
      this.clearConfirmationTimer(record);
      record.error = CONFIRMATION_EXPIRED;
      this.publish(record);
      return;
    }
    if (confirmation.deadline <= this.now()) {
      record.confirmation = null;
      this.clearConfirmationTimer(record);
      record.error = CONFIRMATION_EXPIRED;
      this.publish(record);
      return;
    }
    if (confirmation.taskIds.length === 0) {
      record.error = NOTHING_TO_CANCEL;
      this.publish(record);
      return;
    }

    const operation = ++this.operation;
    const epoch = record.previewEpoch;
    const token = confirmation.token;
    record.confirmPending = true;
    record.confirmOperation = operation;
    record.confirmTimedOut = false;
    record.error = null;
    this.clearConfirmTimer(record);
    record.confirmTimer = setTimeout(() => {
      record.confirmTimer = null;
      if (record.confirmOperation !== operation || !record.confirmPending) return;
      record.confirmTimedOut = true;
      record.notice = WAITING_CONFIRM;
      this.publish(record);
    }, ACTION_WAIT_HINT_MS);
    this.publish(record);

    try {
      await connection!.client.request(CANCEL, {
        threadId: target.threadId,
        taskId: target.taskId,
        confirmationToken: token,
      });
    } catch (error) {
      this.settleConfirm(record, operation, epoch, token, () => {
        record.confirmation = null;
        this.clearConfirmationTimer(record);
        record.error = formatError(error) === CANCEL_FAILED
          ? CANCEL_FAILED
          : `${CANCEL_FAILED}（${formatError(error)}）`;
      });
      throw error;
    }

    this.settleConfirm(record, operation, epoch, token, () => {
      record.confirmation = null;
      this.clearConfirmationTimer(record);
      record.notice = MOTD_CANCELLED;
    });
  }

  /**
   * Settle a confirm without letting it reach a view it was not issued for.
   *
   * The pending flag is per record, so it is always cleared for the owning
   * record; everything visible (notice, error, the token itself) is published
   * only while the epoch still matches.
   */
  private settleConfirm(
    record: TaskRecord,
    operation: number,
    epoch: number,
    token: string,
    outcome: () => void,
  ): void {
    if (record.confirmOperation !== operation) return;
    record.confirmPending = false;
    this.clearConfirmTimer(record);
    if (record.previewEpoch !== epoch) {
      this.publish(record);
      return;
    }
    if (record.confirmation?.token === token) {
      record.confirmation = null;
      this.clearConfirmationTimer(record);
    }
    outcome();
    this.publish(record);
  }

  // ---------------------------------------------------------------------------
  // Connection and lifecycle
  // ---------------------------------------------------------------------------

  /**
   * The connection carrying the pending tokens has died.
   *
   * Tokens die at once (they can no longer be confirmed) and in-flight cancel
   * replies become stale, but drafts and the pending guards stay as they are
   * until the transport actually settles them.
   */
  disconnected(connection: TraceConnection): void {
    for (const record of this.records.values()) {
      const confirmation = record.confirmation;
      if (confirmation === null || confirmation.client !== connection.client) continue;
      if (confirmation.connectionGeneration !== connection.connectionGeneration) continue;
      record.previewEpoch += 1;
      record.confirmation = null;
      this.clearConfirmationTimer(record);
      this.publish(record);
    }
  }

  dropThread(slot: string, bindingGeneration: number, threadId: string): void {
    this.dropWhere((target) => target.slot === slot
      && target.serverBindingGeneration === bindingGeneration
      && target.threadId === threadId);
  }

  dropSlot(slot: string): void {
    this.dropWhere((target) => target.slot === slot);
  }

  clear(): void {
    for (const key of [...this.records.keys()]) this.deleteRecord(key);
    this.visibleKey = null;
  }

  private dropWhere(matches: (target: TraceTarget) => boolean): void {
    for (const [key, record] of [...this.records]) {
      if (!matches(record.target)) continue;
      this.deleteRecord(key);
    }
  }

  /** Invalidate the lifetime first, then forget the record. */
  private deleteRecord(key: string): void {
    const record = this.records.get(key);
    if (!record) return;
    record.previewEpoch += 1;
    record.confirmation = null;
    this.clearMessageTimer(record);
    this.clearPreviewTimer(record);
    this.clearConfirmTimer(record);
    this.clearConfirmationTimer(record);
    this.records.delete(key);
    if (this.visibleKey === key) this.visibleKey = null;
    for (const listener of [...this.listeners]) listener();
  }

  // ---------------------------------------------------------------------------
  // Internals
  // ---------------------------------------------------------------------------

  private matches(target: TraceTarget, connection: TraceConnection): boolean {
    return connection.slot === target.slot
      && connection.serverBindingGeneration === target.serverBindingGeneration;
  }

  private clearMessageTimer(record: TaskRecord): void {
    if (record.messageTimer === null) return;
    clearTimeout(record.messageTimer);
    record.messageTimer = null;
  }

  private clearPreviewTimer(record: TaskRecord): void {
    if (record.previewTimer === null) return;
    clearTimeout(record.previewTimer);
    record.previewTimer = null;
  }

  private clearConfirmTimer(record: TaskRecord): void {
    if (record.confirmTimer === null) return;
    clearTimeout(record.confirmTimer);
    record.confirmTimer = null;
  }

  private clearConfirmationTimer(record: TaskRecord): void {
    if (record.confirmationTimer === null) return;
    clearTimeout(record.confirmationTimer);
    record.confirmationTimer = null;
  }

  /** Rebuild the public snapshot only when something a reader can see changed. */
  private publish(record: TaskRecord): void {
    const confirmation = record.confirmation;
    const next: TraceActionSnapshot = {
      draft: record.draft,
      draftRevision: record.draftRevision,
      messagePending: record.messagePending,
      previewPending: record.previewPending,
      confirmPending: record.confirmPending,
      confirmation: confirmation === null
        ? null
        : { token: confirmation.token, taskIds: confirmation.taskIds, deadline: confirmation.deadline },
      error: record.error,
      notice: record.notice,
    };
    const previous = record.snapshot;
    if (
      previous.draft === next.draft
      && previous.draftRevision === next.draftRevision
      && previous.messagePending === next.messagePending
      && previous.previewPending === next.previewPending
      && previous.confirmPending === next.confirmPending
      && previous.error === next.error
      && previous.notice === next.notice
      && previous.confirmation?.token === next.confirmation?.token
      && previous.confirmation?.deadline === next.confirmation?.deadline
      && sameIds(previous.confirmation?.taskIds ?? [], next.confirmation?.taskIds ?? [])
    ) {
      return;
    }
    record.snapshot = next;
    for (const listener of [...this.listeners]) listener();
  }
}
