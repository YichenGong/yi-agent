/**
 * Handshake-time sync verdicts: turning the server's `syncPlan` (plus the
 * listing's version fields as a fallback) into one action per thread.
 *
 * The whole point is that the client must not *guess* whether its cache is
 * still valid: the server knows which threads it deleted and whether the
 * client's anchor still exists in its log, and says so explicitly.
 */
import type { ContentEntry } from "./contentStore";
import type { ThreadSummary } from "./protocol";

export type SyncAction = "full" | "none" | "drop";

/**
 * Parse `initialize`'s result into a per-thread verdict.
 *
 * Unknown `action` values are dropped rather than guessed at: an unrecognized
 * verdict must leave the thread on the conservative fallback path. The `epoch`
 * is a best-effort value (defaulting to 0 when absent) used only as a fallback
 * comparison when the plan does not cover a thread.
 */
export function parseSyncPlan(
  result: unknown,
): Map<string, { action: SyncAction; epoch: number }> {
  const out = new Map<string, { action: SyncAction; epoch: number }>();
  const raw = (result as { syncPlan?: unknown } | null | undefined)?.syncPlan;
  if (!Array.isArray(raw)) return out;
  for (const e of raw) {
    const threadId = (e as { threadId?: unknown })?.threadId;
    const action = (e as { action?: unknown })?.action;
    if (typeof threadId !== "string") continue;
    if (action !== "full" && action !== "none" && action !== "drop") continue;
    const epoch = (e as { epoch?: unknown })?.epoch;
    out.set(threadId, { action, epoch: typeof epoch === "number" ? epoch : 0 });
  }
  return out;
}

/**
 * Resolve the action for one thread.
 *
 * Precedence (spec §4.2): when the server's plan covers this thread, its
 * verdict is authoritative — the client cannot itself know whether the thread
 * still exists or whether its anchor is still in the server's log, and those
 * two facts are exactly what distinguish `full` from `none`. The local epoch
 * comparison is only the fallback for threads the plan does not cover, where
 * `thread/listAll`'s `content_epoch` is the server's version; an unknown
 * version on either side degrades to a conservative full resume (a missing
 * field is never read as "consistent" — spec §4.3).
 *
 * The one client-side override: a *trimmed* cache entry is incomplete even
 * when the server (correctly) says `none`, so it must still be replayed in
 * full — otherwise the user silently sees a truncated conversation. This only
 * depends on what the client itself knows (whether it trimmed), so it needs
 * nothing from the wire (spec §4.2).
 */
export function resolveSyncPlan(opts: {
  plan: Map<string, { action: SyncAction; epoch: number }>;
  summary?: ThreadSummary | null;
  cached?: ContentEntry | null;
}): SyncAction {
  // `epoch` 只在 plan 未覆盖该会话时用作兜底比较；plan 覆盖时一律以服务端
  // 裁决为准（服务端已经用 hint 里的 epoch 与真实日志算过）。
  const threadId = opts.summary?.thread_id;
  const fromPlan = threadId === undefined ? undefined : opts.plan.get(threadId);

  let verdict: SyncAction;
  if (fromPlan) {
    verdict = fromPlan.action;
  } else {
    const serverEpoch =
      typeof opts.summary?.content_epoch === "number" ? opts.summary.content_epoch : undefined;
    const cachedEpoch = opts.cached?.contentEpoch;
    verdict =
      serverEpoch === undefined || cachedEpoch == null
        ? "full" // 无从裁量 ⇒ 保守全量（缺字段绝不当作"一致"）
        : cachedEpoch === serverEpoch
          ? "none"
          : "full";
  }

  // 客户端例外：本地缓存被裁剪过 ⇒ 即使裁决是 none 也必须全量补齐头部。
  if (verdict === "none" && opts.cached?.trimmed) return "full";
  return verdict;
}
