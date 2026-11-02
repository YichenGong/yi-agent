import { formatRelativeTime, RELATIVE_TIME_UNKNOWN } from "../lib/relativeTime";

/**
 * The strip shown above the transcript while it is not (yet) the server's
 * authoritative content. Two mutually exclusive states:
 *
 * - `syncing`: the catch-up is in flight on a live connection — the cached
 *   transcript is on screen and may be stale, so sending is blocked meanwhile.
 * - `failed`: the reconciliation failed (server rejection, or a stalled
 *   replay). Saying "connection down" here would be a lie — the connection is
 *   often perfectly healthy — so it must name the real reason.
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
  failed,
}: {
  savedAt?: number | null;
  syncing?: boolean;
  /** 对账失败：此时说「连接断开」是错的（连接可能好着），要说出真正的原因。 */
  failed?: boolean;
}) {
  return (
    <div
      role="status"
      className="border-b border-line bg-raised px-4 py-1 text-xs text-fg-muted"
    >
      {/* 顺序是刻意的：`failed` 压过 `syncing`。两者同为真是状态机自身的问题
          （置 failed 时必然已离开 syncing），真撞上时优先说出"失败了、可以重试"
          ——那是用户唯一能行动的结论。 */}
      {failed ? "内容加载失败 · 可重试" : syncing ? "正在同步…" : offlineLabel(savedAt)}
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
