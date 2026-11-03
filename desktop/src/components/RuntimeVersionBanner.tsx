/**
 * The banner that says "the background runtime is a different build".
 *
 * It is the only place a version switch can be started from, and it is
 * deliberately not a modal: the app keeps working while a compatible mismatch is
 * on screen. Only `controlOnly` disables business operation, and that is enforced
 * by the runtime, not by hiding the chat window.
 */

import { useRuntimeLifecycle } from "../hooks/useRuntimeLifecycle";
import type { LifecycleState, RuntimeControl } from "../lib/runtimeLifecycle";
import { summarise } from "../lib/runtimeLifecycle";

/**
 * The container: owns the lifecycle for one scope and passes it down.
 *
 * The scope is a prop, not derived here, because only the shell can say which
 * runtime directory a project maps to. Switching scopes unmounts the machine (see
 * `useRuntimeLifecycle`), so a stale preview token cannot cross projects.
 */
export function RuntimeVersionBanner({
  scopeKey,
  control,
  readOperationId,
}: {
  scopeKey: string;
  control: RuntimeControl;
  readOperationId?: (scopeKey: string) => Promise<string | null>;
}) {
  const lifecycle = useRuntimeLifecycle({ scopeKey, control, readOperationId });
  return (
    <RuntimeVersionBannerView
      state={lifecycle.state}
      onRestart={() => void lifecycle.prepare()}
      onConfirm={() => void lifecycle.confirm()}
      onCancel={lifecycle.cancel}
      onRefresh={() => void lifecycle.refresh()}
    />
  );
}

export interface RuntimeVersionBannerProps {
  state: LifecycleState;
  onRestart: () => void;
  onConfirm: () => void;
  onCancel: () => void;
  onRefresh: () => void;
}

/**
 * The presentational half.
 *
 * Split out so the rendering rules -- what a legacy runtime is allowed to claim,
 * what an incomplete count is allowed to say -- are testable from a literal state
 * object, without a fake clock driving a poll loop.
 */
/** Renders nothing when there is nothing to say, which keeps App.tsx a list. */
export function RuntimeVersionBannerView({
  state,
  onRestart,
  onConfirm,
  onCancel,
  onRefresh,
}: RuntimeVersionBannerProps) {
  const status = state.status;
  if (status === null) return null;
  const summary = summarise(status);
  if (!summary.visible) return null;

  // While a switch is running, the banner's job changes from "tell" to "wait".
  if (state.phase === "confirming" || state.phase === "switching" || state.phase === "finished") {
    const failed = state.operation?.phase === "failed";
    const manual = state.operation?.phase === "manual_action_required";
    return (
      <div
        role="status"
        aria-live="polite"
        className="flex flex-wrap items-center gap-3 border-b border-sky-900/60 bg-sky-950/60 px-3 py-2 text-xs text-sky-100"
      >
        <span className="min-w-0">
          {failed
            ? "the runtime could not be restarted"
            : manual
              ? "the runtime needs a manual step: restart yi-agent"
              : "restarting the background runtime…"}
        </span>
        {state.operation?.detail && (
          <span className="min-w-0 truncate text-sky-200/80">{state.operation.detail}</span>
        )}
        {(failed || manual) && (
          <button
            type="button"
            onClick={onRefresh}
            className="rounded bg-sky-800/70 px-2 py-0.5 hover:bg-sky-700/70"
          >
            Refresh
          </button>
        )}
      </div>
    );
  }

  if (state.phase === "unknown") {
    return (
      <div
        role="status"
        aria-live="polite"
        className="flex flex-wrap items-center gap-3 border-b border-amber-900/60 bg-amber-950/60 px-3 py-2 text-xs text-amber-100"
      >
        <span className="min-w-0">{state.error ?? "the runtime state is unknown"}</span>
        <button
          type="button"
          onClick={onRefresh}
          className="rounded bg-amber-800/70 px-2 py-0.5 hover:bg-amber-700/70"
        >
          Refresh
        </button>
      </div>
    );
  }

  // A preview is on screen: this is the decision, not an announcement.
  if (state.preview) {
    return <RuntimeRestartDialog state={state} onConfirm={onConfirm} onCancel={onCancel} />;
  }

  return (
    <div
      role="status"
      aria-live="polite"
      className="flex flex-wrap items-center gap-3 border-b border-amber-900/60 bg-amber-950/60 px-3 py-2 text-xs text-amber-100"
    >
      <span className="min-w-0">
        {summary.currentVersion
          ? `Runtime ${summary.currentVersion}`
          : "The background runtime could not be identified"}
        {summary.expectedVersion ? ` · app expects ${summary.expectedVersion}` : ""}
        {" — "}
        {summary.message}
      </span>
      {state.controlUnavailable ? (
        <span className="min-w-0 text-amber-200/80">
          restart is not available from this app; update both the app and the runtime
        </span>
      ) : summary.canRestart ? (
        <button
          type="button"
          onClick={onRestart}
          disabled={state.busy}
          className="rounded bg-amber-800/70 px-2 py-0.5 hover:bg-amber-700/70 disabled:opacity-50"
        >
          Restart runtime
        </button>
      ) : (
        <span className="min-w-0 text-amber-200/80">{summary.cannotRestartReason}</span>
      )}
      {state.error && <span className="min-w-0 text-red-300">{state.error}</span>}
    </div>
  );
}

/**
 * The confirmation, inline rather than modal.
 *
 * It lists what will be interrupted by name, because "2 clients" is not enough to
 * decide with: the user needs to recognise the terminal they left running.
 */
function RuntimeRestartDialog({
  state,
  onConfirm,
  onCancel,
}: {
  state: LifecycleState;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  const preview = state.preview;
  if (!preview) return null;
  // Named separately from `needsSecondConfirmation` on purpose: that function
  // answers "must we ask?", this one answers "must the button say 'anyway'?", and
  // an incomplete count is a reason to ask but not a reason to shout.
  const second =
    preview.counts.complete &&
    (preview.counts.clients > 1 || preview.counts.pendingClients > 0 || preview.counts.running > 0);
  return (
    <div
      role="dialog"
      aria-label="Restart the background runtime"
      className="border-b border-amber-900/60 bg-amber-950/60 px-3 py-2 text-xs text-amber-100"
    >
      <p className="mb-2">
        Restarting the background runtime stops whatever is attached to it.{" "}
        {preview.counts.running > 0n
          ? `${preview.counts.running} task(s) are running and will be interrupted.`
          : preview.counts.complete
            ? "No task is running."
            : "Running tasks could not be counted; assume some are."}
      </p>
      <ul className="mb-2 space-y-0.5">
        {preview.entries.slice(0, 8).map((entry) => (
          <li key={`${entry.kind}-${entry.id}`} className="min-w-0 truncate">
            {entry.kind} {entry.id}
            {entry.owner ? ` (${entry.owner})` : ""}
          </li>
        ))}
        {preview.entries.length > 8 && <li>…and {preview.entries.length - 8} more</li>}
      </ul>
      {second && (
        <p className="mb-2 text-amber-200">
          This will also interrupt other clients connected to this project.
        </p>
      )}
      {preview.blockedReason && <p className="mb-2 text-red-300">{preview.blockedReason}</p>}
      <div className="flex gap-2">
        <button
          type="button"
          onClick={onConfirm}
          disabled={!preview.canRestart || state.busy}
          className="rounded bg-amber-700/80 px-2 py-0.5 hover:bg-amber-600/80 disabled:opacity-50"
        >
          {second ? "Restart anyway" : "Restart"}
        </button>
        <button
          type="button"
          onClick={onCancel}
          className="rounded bg-stone-800/80 px-2 py-0.5 hover:bg-stone-700/80"
        >
          Cancel
        </button>
      </div>
    </div>
  );
}
