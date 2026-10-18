import { formatRelativeTime } from "../lib/relativeTime";

/**
 * The strip shown above the transcript while it is not (yet) the server's
 * authoritative content. Two mutually exclusive states:
 *
 * - `syncing`: the catch-up is in flight on a live connection — the cached
 *   transcript is on screen and may be stale, so sending is blocked meanwhile.
 * - otherwise (offline): the connection is down; `savedAt` says how old the
 *   cached content is.
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
      {syncing ? "正在同步…" : `离线内容 · 更新于 ${formatRelativeTime(savedAt ?? 0)}`}
    </div>
  );
}
