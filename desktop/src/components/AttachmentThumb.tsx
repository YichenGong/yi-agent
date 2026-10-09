import { useImageData, type ImageReadCall } from "../lib/useImageData";

/**
 * 待发图片 chip 上的缩略图。
 *
 * 刻意做成一个叶子组件：`useImageData` 必须**无条件**调用，而 `AttachmentChips`
 * 是在 `map` 里按 `kind` 分支渲染的——把 hook 放进那个分支里就违反了 Hook 规则。
 * 这里一个组件只服务一个 `path`，hook 自然总在最外层。
 */
export function AttachmentThumb({
  path,
  threadId,
  call,
}: {
  path: string;
  threadId: string | null;
  call: ImageReadCall;
}) {
  const { url, error } = useImageData(path, { threadId, call });

  if (error) {
    // 读不回来就不画破图：留一个小标记（chip 上的文件名与移除按钮照旧可用）。
    return (
      <span
        data-testid="attachment-thumb-error"
        title={`图片读取失败：${error}`}
        className="flex h-10 w-10 shrink-0 items-center justify-center rounded border border-red-400/40 bg-red-500/10 text-[10px] text-red-300"
      >
        ×
      </span>
    );
  }

  if (!url) {
    // 字节还在路上（或没有会话）：给一个中性占位块，占住同一处布局而不闪破图。
    return (
      <span
        data-testid="attachment-thumb"
        className="h-10 w-10 shrink-0 rounded border border-line bg-surface"
      />
    );
  }

  return (
    <img
      src={url}
      // 空 alt：文件名就在旁边的 chip 文本里，图片本身只是补充视觉信息。
      alt=""
      data-testid="attachment-thumb"
      className="h-10 w-10 shrink-0 rounded object-cover"
    />
  );
}
