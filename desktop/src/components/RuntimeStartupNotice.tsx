/**
 * The banner that says "this project's local runtime is not ready".
 *
 * It exists because the old behaviour was silence: a preparation refused by the
 * shell, or a runtime that never answered, looked exactly like a project that was
 * fine -- until a conversation was created with no tools and no explanation.
 *
 * Two rules shape the rendering:
 *
 * * the reason is chosen from a fixed table (`startupIssueReason`), never taken
 *   from the native reply, which can name paths or carry an error string;
 * * the retry button is desktop-only. A paired remote client has no local shell,
 *   so offering one would send a command that cannot exist.
 */

import type { RuntimeStartupIssue } from "../lib/runtimeStartupIssue";

export interface RuntimeStartupNoticeProps {
  /** Preparation problems, keyed by project path. */
  issues: ReadonlyMap<string, RuntimeStartupIssue>;
  /** The project on screen, if any. Only its issue is shown. */
  project: string | null;
  /** Whether this client may run local commands (desktop, not a paired remote). */
  local: boolean;
  /** Asks the shell to prepare the project again. Never sends a turn. */
  onRetry: (project: string) => void;
  /** Switches the conversation to the project that is having the problem. */
  onOpenProject?: (project: string) => void;
}

/** Renders nothing when the project on screen has no problem. */
export function RuntimeStartupNotice({
  issues,
  project,
  local,
  onRetry,
  onOpenProject,
}: RuntimeStartupNoticeProps) {
  if (project === null) return null;
  const issue = issues.get(project);
  if (!issue) return null;
  return (
    <div className="runtime-startup-notice" role="alert">
      <span className="runtime-startup-notice__text">{issue.reason}</span>
      {local && (
        <button
          type="button"
          className="runtime-startup-notice__retry"
          onClick={() => onRetry(project)}
        >
          重试
        </button>
      )}
      {!local && onOpenProject && (
        <button
          type="button"
          className="runtime-startup-notice__open"
          onClick={() => onOpenProject(project)}
        >
          查看项目
        </button>
      )}
    </div>
  );
}
