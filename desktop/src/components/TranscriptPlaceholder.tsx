/**
 * 转录区在**没有内容**时显示的三种态。
 *
 * 存在的理由：手机 `body` 底色是深色（`index.css` 的 `--panel`），转录区为空
 * 与「一片黑」在视觉上无法区分。用户看到黑屏时既不知道在不在加载、也不知道
 * 能不能重试。这个组件把那三种「本该有内容但没有」的情形说清楚。
 *
 * 只做展示：不读 store、不发 RPC、不持有状态。重试由宿主注入。
 */
export type PlaceholderKind = "loading" | "failed" | "crashed";

const SKELETON_WIDTHS = ["88%", "64%", "78%", "42%"];

export function TranscriptPlaceholder({
  kind,
  reason,
  onRetry,
}: {
  kind: PlaceholderKind;
  /** 失败原因（已格式化）。仅 failed / crashed 使用。 */
  reason?: string | null;
  /** 重试回调。不传则不渲染按钮（`crashed` 的复位由宿主负责）。 */
  onRetry?: () => void;
}) {
  if (kind === "loading") {
    return (
      <div
        role="status"
        className="flex flex-col gap-3 px-1 py-3"
        aria-live="polite"
      >
        <span className="sr-only">正在加载会话内容</span>
        <div data-testid="sync-skeleton" aria-hidden="true" className="flex flex-col gap-3">
          {SKELETON_WIDTHS.map((width, i) => (
            <div
              key={i}
              className="h-4 rounded-md bg-raised motion-safe:animate-pulse"
              style={{ width }}
            />
          ))}
        </div>
      </div>
    );
  }

  const crashed = kind === "crashed";
  return (
    <div
      data-testid={crashed ? "sync-crashed" : "sync-failed"}
      className="flex flex-col items-center gap-3 px-4 py-10 text-center"
    >
      <div className="text-sm font-medium text-fg">
        {crashed ? "这段内容渲染失败" : "内容加载失败"}
      </div>
      {reason ? (
        <p className="max-w-sm break-words text-xs text-fg-muted">{reason}</p>
      ) : null}
      {onRetry ? (
        <button
          type="button"
          onClick={onRetry}
          className="rounded-md border border-line-strong px-4 py-2 text-sm hover:bg-raised"
        >
          重试
        </button>
      ) : null}
      {crashed ? (
        <p className="max-w-sm text-xs text-fg-subtle">
          侧栏和输入框还可以用：可以切到其它会话，或重新连接。
        </p>
      ) : null}
    </div>
  );
}
