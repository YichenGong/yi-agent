import { formatRelativeTime, RELATIVE_TIME_UNKNOWN } from "../lib/relativeTime";

/**
 * The strip shown above the transcript while it is not (yet) the server's
 * authoritative content. Two mutually exclusive states:
 *
 * - `syncing`: the catch-up is in flight on a live connection — the cached
 *   transcript is on screen and may be stale, so sending is blocked meanwhile.
 * - otherwise (offline): the connection is down; `savedAt` says how old the
 *   cached content is, or is unknown.
 *
 * Its only job is to stop a stale transcript (and its stale `running` badge)
 * from being read as the current truth.
 */
export function CacheNotice({
  savedAt,
  syncing,
}: {
  savedAt?: number | null;
  syncing?: boolean;
}) {
  return (
    <div
      role="status"
      className="border-b border-line bg-raised px-4 py-1 text-xs text-fg-muted"
    >
      {syncing ? "正在同步…" : offlineLabel(savedAt)}
    </div>
  );
}

/**
 * 离线段的那句话。
 *
 * 缺值**不能**兜成 0：0 是 1970 年，格式化出来是 20758d（用户报的「20xxxd」）。
 * 时间算不出来时如实说「更新时间未知」——比一个谁也解释不了的天数诚实，也更短。
 */
function offlineLabel(savedAt: number | null | undefined): string {
  const age = savedAt == null ? RELATIVE_TIME_UNKNOWN : formatRelativeTime(savedAt);
  return age === RELATIVE_TIME_UNKNOWN
    ? "离线内容 · 更新时间未知"
    : `离线内容 · 更新于 ${age}`;
}
