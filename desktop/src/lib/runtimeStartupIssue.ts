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
   * A user-facing reason. For a shell refusal this is the shell's own text; for
   * a thrown command it is a fixed sentence, never the raw exception (which can
   * carry paths or credential-shaped values).
   */
  reason: string;
}

/** Whether a reply still answers the request currently on the wire. */
export function acceptsStartupReply(currentSeq: number, replySeq: number): boolean {
  return currentSeq === replySeq;
}

/** The fixed reason used when the shell command itself failed. */
export const ACQUIRE_FAILED_REASON =
  "本机运行环境准备失败；可重试，若仍失败请检查本机运行时日志";
