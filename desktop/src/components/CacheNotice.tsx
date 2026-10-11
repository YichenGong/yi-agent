import { formatRelativeTime } from "../lib/relativeTime";

/**
 * The "this is cached, not live" strip shown above the transcript while the
 * phone client is offline. Its only job is to stop a stale transcript (and its
 * stale `running` badge) from being read as the current truth.
 */
export function CacheNotice({ savedAt }: { savedAt: number }) {
  return (
    <div
      role="status"
      className="border-b border-line bg-raised px-4 py-1 text-xs text-fg-muted"
    >
      离线内容 · 更新于 {formatRelativeTime(savedAt)}
    </div>
  );
}
