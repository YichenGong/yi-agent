/**
 * One project's runtime-preparation problem, in a shape the page may render.
 *
 * The native shell owns the wording of a refusal (it knows whether a lease was
 * refused, a version mismatch was found, or an answer was lost). This module
 * only decides which project the problem belongs to and whether a reply is
 * still current, so a late answer cannot overwrite a newer project's state.
 */

export type StartupStage = "acquire" | "daemon" | "root" | "tooling";

export interface RuntimeStartupIssue {
  project: string;
  /** Monotonic per project; a reply for an older request is discarded. */
  requestSeq: number;
  attemptId?: string;
  scopeKey?: string;
  daemonInstanceId?: string;
  stage: StartupStage;
  /** The native state that produced this issue, for wording and retry policy. */
  state?: string;
  /**
   * A user-facing sentence from a fixed table keyed by the native state.
   *
   * Never the native reason verbatim and never an exception's text: both can
   * carry paths or credential-shaped values.
   */
  reason: string;
}

/**
 * The user-facing wording for one preparation state.
 *
 * Native reasons are diagnostic, not presentational: they can name absolute
 * paths, the runtime directory, or an error string from a socket. Rendering one
 * verbatim would leak that into the page, so every state maps to a fixed
 * sentence and an unrecognised state gets the generic one. `null` means the
 * state is not a problem at all.
 */
export function startupIssueReason(state: string | undefined): string | null {
  switch (state) {
    case undefined:
    case "managedReady":
      return null;
    case "starting":
      return "本机运行环境正在准备，请稍候";
    case "legacyChoiceRequired":
      return "此项目由手工启动的旧运行时服务，需要你选择如何处理";
    case "blocked":
      return "本机运行环境未能就绪，项目暂时无法运行工具";
    case "unknown":
      return "无法确认本机运行环境状态，可重试一次";
    default:
      return ACQUIRE_FAILED_REASON;
  }
}

/**
 * Whether a state needs the user's attention at all.
 *
 * A missing state means the shell command itself failed (it threw, or answered
 * with nothing) rather than reporting a healthy runtime. That is still a
 * problem: treating it as "ready" would swallow the only evidence the user has
 * that nothing was prepared.
 */
export function isStartupProblem(state: string | undefined): boolean {
  if (state === undefined) return true;
  return startupIssueReason(state) !== null;
}

/** Whether a reply still answers the request currently on the wire. */
export function acceptsStartupReply(currentSeq: number, replySeq: number): boolean {
  return currentSeq === replySeq;
}

/** The fixed reason used when the shell command itself failed. */
export const ACQUIRE_FAILED_REASON =
  "本机运行环境准备失败；可重试，若仍失败请检查本机运行时日志";
