/**
 * The chat view's version notice: this thread's runtime is a different build.
 *
 * Separate from `RuntimeVersionBanner` because the two answer different questions.
 * That one is about *this shell* and offers a restart. This one is about *the
 * runtime a thread is attached to*, which may be a remote project or a runtime this
 * shell does not control; its whole job is to say it plainly and point at the
 * desktop app. Folding them together would put a restart button next to a
 * connection this app cannot restart.
 */

import type { Compatibility } from "../lib/generated/Compatibility";
import { describeStatus, type RuntimeStatusView } from "../lib/runtimeLifecycle";

export interface VersionMismatchBannerProps {
  status: RuntimeStatusView | null;
  /** True when this build is the paired remote client. */
  remote: boolean;
  /** Called when the user asks why business operation is disabled. */
  onExplain?: () => void;
}

export function VersionMismatchBanner({ status, remote, onExplain }: VersionMismatchBannerProps) {
  if (status === null || status.compatibility === "sameBuild") return null;
  const controlOnly = status.compatibility === "controlOnly";
  const classes = controlOnly
    ? "border-red-900/60 bg-red-950/60 text-red-100"
    : "border-amber-900/60 bg-amber-950/60 text-amber-100";
  return (
    <div
      role="status"
      aria-live="polite"
      className={`flex flex-wrap items-center gap-3 border-b px-3 py-2 text-xs ${classes}`}
    >
      <span className="min-w-0">{describeStatus(status)}</span>
      {status.identity.daemonDisplayVersion && status.expectedDisplayVersion && (
        <span className="min-w-0 text-current/70">
          {status.identity.daemonDisplayVersion} → {status.expectedDisplayVersion}
        </span>
      )}
      {controlOnly && onExplain && (
        <button
          type="button"
          onClick={onExplain}
          className="rounded bg-red-800/70 px-2 py-0.5 hover:bg-red-700/70"
        >
          Why are tasks disabled?
        </button>
      )}
      {remote && (
        <span className="min-w-0 opacity-80">
          restart it from the desktop app; this device cannot
        </span>
      )}
    </div>
  );
}

/** Re-exported so call sites do not import the generated type directly. */
export type { Compatibility };
