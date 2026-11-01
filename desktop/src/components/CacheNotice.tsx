import { formatRelativeTime, RELATIVE_TIME_UNKNOWN } from "../lib/relativeTime";

/**
 * The strip shown above the transcript while it is not (yet) the server's
 * authoritative content. Two mutually exclusive states:
 *
 * - `syncing`: the catch-up is in flight on a live connection — the cached
 *   transcript is on screen and may be stale, so sending is blocked meanwhile.
 * - otherwise (offline): the connection is down; `savedAt` says how old the
 *   cached content is. A missing `savedAt` means there is **no** cached
 *   content at all, so the strip must not claim to be showing one.
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
 * 两个坑都要躲开：
 * ① 缺值**不能**兜成 0：0 是 1970 年，格式化出来是 20758d（用户报的「20xxxd」）。
 * ② 只说屏幕上真实存在的东西。`savedAt` 为 null 并不是「有缓存但不知道多旧」，
 *    而是「**没有**缓存条目」——`cachedAt` 只在读到缓存时才写入。此时正文区是空
 *    的，若仍写「离线内容 · 更新时间未知」，就是在介绍一份用户看不到的缓存。
 *    所以这里如实说「内容未缓存」，同时不编造任何时间。
 */
function offlineLabel(savedAt: number | null | undefined): string {
  if (savedAt == null) return "内容未缓存 · 连接断开";
  const age = formatRelativeTime(savedAt);
  return age === RELATIVE_TIME_UNKNOWN
    ? "内容未缓存 · 连接断开"
    : `离线内容 · 更新于 ${age}`;
}
