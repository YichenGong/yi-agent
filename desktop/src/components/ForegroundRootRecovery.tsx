import { useRef, useState } from "react";

/**
 * What a recovery would stop, as the shell described it.
 *
 * Mirrors `RecoveryImpactView` in `desktop/src-tauri/src/foreground_root.rs`
 * field for field: the two are one wire contract, and a field added on one side
 * without the other is a bug the type system cannot see across the process line.
 */
export type RecoveryImpactView = {
  rootId: string;
  attemptId: string;
  stoppedExecutionIds: string[];
  blockedExecutionIds: string[];
  childIds: string[];
  version: number;
  reason: string;
};

/**
 * A prepared recovery.
 *
 * `token` is `null` while the daemon withheld one: a blocked preparation must not
 * render as confirmable, which is why the button also checks `blocked`.
 */
export type RecoveryPreviewView = {
  operationId: string;
  token: string | null;
  blocked: string[];
  expiresAt: string | null;
  impact: RecoveryImpactView;
};

/** The durable, secret-free identity of a committed recovery. */
export type RecoveryResultView = {
  operationId: string;
  previousRootId: string;
  previousAttemptId: string;
  successorAttemptId: string;
  bindingEpoch: number;
  driverEpoch: number;
};

/**
 * One operation's state.
 *
 * A tagged union because only `committed` may ever be rendered as success:
 * `preparing` and `needsPreview` mean the daemon has not decided yet, and a UI
 * that folded them into success would report a repair that never happened.
 */
export type RecoveryOperationView =
  | { phase: "preparing"; operationId: string }
  | { phase: "needsPreview"; operationId: string }
  | { phase: "blocked"; operationId: string; impact: RecoveryImpactView }
  | { phase: "ready"; operationId: string; preview: RecoveryPreviewView }
  | { phase: "committed"; result: RecoveryResultView };

/**
 * Asks the panel to commit the prepared operation.
 *
 * The component never mints an operation id: every call names the one the preview
 * already carries, so a retry can only ever be about the same recovery.
 */
export type ConfirmRecovery = (
  operationId: string,
  token: string,
) => Promise<RecoveryOperationView>;

/** Reads one operation's stored state. `null` = the runtime never ran that id. */
export type QueryOperation = (
  operationId: string,
) => Promise<RecoveryOperationView | null>;

/** Parses an expiry into a comparable instant, or `null` when it is unusable. */
function parseExpiry(expiresAt: string | null): number | null {
  if (!expiresAt) return null;
  const parsed = Date.parse(expiresAt);
  return Number.isFinite(parsed) ? parsed : null;
}

/**
 * One sentence per phase, so the reason a button is disabled is never a mystery.
 *
 * Kept as a lookup rather than inline ternaries: the phases are a closed set, and a
 * new variant should fail to type-check here rather than silently render "unknown".
 */
function phaseLabel(operation: RecoveryOperationView): string {
  switch (operation.phase) {
    case "preparing":
      return "正在停止旧执行…";
    case "needsPreview":
      return "运行时已重启，需要重新准备恢复";
    case "blocked":
      return "仍有执行未能证明停止，无法确认恢复";
    case "ready":
      return "旧执行已停止，可以确认恢复";
    case "committed":
      return "已完成恢复";
  }
}

/**
 * Confirms a prepared foreground-root recovery.
 *
 * The safety properties live in this component, not in its props:
 *
 * * a blocked or tokenless preview cannot be confirmed at all;
 * * a missing or unparseable expiry is treated as expired, never as "no deadline";
 * * the first click *queries* the operation, so a retry after a lost answer
 *   resolves against the same id instead of starting a second recovery;
 * * double clicks are stopped by a synchronous ref, because two awaits can both
 *   read a stale `confirming` state.
 */
export function ForegroundRootRecovery({
  preview,
  confirmRecovery,
  queryOperation,
  cancel,
}: {
  preview: RecoveryPreviewView;
  confirmRecovery: ConfirmRecovery;
  queryOperation: QueryOperation;
  cancel: () => void;
}) {
  const inFlight = useRef(false);
  const [confirming, setConfirming] = useState(false);
  const [operation, setOperation] = useState<RecoveryOperationView | null>(null);
  const [failure, setFailure] = useState<string | null>(null);

  const expiry = parseExpiry(preview.expiresAt);
  const expired = expiry === null || Date.now() >= expiry;
  const committable = preview.token !== null && preview.blocked.length === 0 && !expired;

  async function confirmOnce() {
    const token = preview.token;
    if (!token || preview.blocked.length !== 0 || inFlight.current) return;
    if (expiry === null || Date.now() >= expiry) {
      setFailure("确认已过期，请重新准备恢复");
      return;
    }
    inFlight.current = true;
    setConfirming(true);
    setFailure(null);
    try {
      // Query first: an earlier attempt may already have committed, and confirming
      // again would be a second recovery for a root that has a successor.
      const before = await queryOperation(preview.operationId);
      if (!before || before.phase !== "ready") {
        if (before) setOperation(before);
        else setFailure("尚无可确认操作，请查询原操作或重新准备");
        return;
      }
      setOperation(await confirmRecovery(preview.operationId, token));
    } catch {
      // The answer was lost. Ask the same operation rather than minting a new one.
      try {
        const current = await queryOperation(preview.operationId);
        if (current) setOperation(current);
        else setFailure("尚无提交结果，请检查同一操作，勿重复新建恢复");
      } catch {
        setFailure("结果暂未知，请稍后查询同一操作");
      }
    } finally {
      inFlight.current = false;
      setConfirming(false);
    }
  }

  const shown = operation;

  return (
    <div className="flex flex-col gap-2 rounded border border-amber-900/60 bg-amber-950/40 px-3 py-2 text-xs text-amber-100">
      <div className="flex flex-wrap items-baseline gap-2">
        <span className="font-semibold">前台根恢复</span>
        <span className="text-amber-300">{preview.operationId}</span>
      </div>

      <div>{preview.impact.reason}</div>

      <div className="flex flex-wrap gap-x-4 gap-y-1 text-amber-300">
        <span>
          旧 attempt：<span className="text-amber-100">{preview.impact.attemptId}</span>
        </span>
        <span>
          root：<span className="text-amber-100">{preview.impact.rootId}</span>
        </span>
        <span>将停止：{preview.impact.stoppedExecutionIds.join("、") || "无"}</span>
        {preview.impact.blockedExecutionIds.length > 0 && (
          <span className="text-red-300">
            未能证明停止：{preview.impact.blockedExecutionIds.join("、")}
          </span>
        )}
        {preview.impact.childIds.length > 0 && (
          <span>受影响子执行：{preview.impact.childIds.join("、")}</span>
        )}
      </div>

      {/* `committed` is rendered by the success block below: two lines saying the
          same thing would read as two separate outcomes. */}
      {shown && shown.phase !== "committed" && (
        <div className="text-amber-200">{phaseLabel(shown)}</div>
      )}

      {shown?.phase === "committed" && (
        <div className="text-emerald-300">
          已完成恢复，新 attempt：<span className="font-mono">{shown.result.successorAttemptId}</span>
        </div>
      )}

      {failure && (
        <div role="alert" className="text-red-300">
          {failure}
        </div>
      )}

      <div className="flex gap-2">
        <button
          type="button"
          onClick={confirmOnce}
          // Disabled while in flight, but the *name* stays stable: a button that
          // renames itself mid-click is both harder to find for a screen reader and
          // impossible to click twice in a test, which hides the double-click guard
          // instead of proving it.
          disabled={!committable || confirming}
          aria-busy={confirming}
          aria-label="确认恢复"
          className="rounded bg-amber-800/70 px-2 py-0.5 hover:bg-amber-700/70 disabled:cursor-not-allowed disabled:opacity-40"
        >
          {confirming ? "确认中…" : "确认恢复"}
        </button>
        <button
          type="button"
          onClick={cancel}
          className="rounded px-2 py-0.5 text-amber-300 hover:text-amber-100"
        >
          取消
        </button>
      </div>
    </div>
  );
}
