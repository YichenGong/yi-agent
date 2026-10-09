import { useImageData, type ImageReadCall } from "../lib/useImageData";

/**
 * 待发图片 chip 上的缩略图。
 *
 * 刻意做成一个叶子组件：`useImageData` 必须**无条件**调用，而 `AttachmentChips`
 * 是在 `map` 里按 `kind` 分支渲染的——把 hook 放进那个分支里就违反了 Hook 规则。
 * 这里一个组件只服务一个 `path`，hook 自然总在最外层。
 *
 * 已知限制：待发（未发送）的本地图片带的是 OS 文件对话框给的**绝对**路径（如
 * `/tmp/截图.png`），而 `image/read` 以会话 cwd 为根（安全边界）并拒绝越界路径，
 * 这类路径因此**必然**读不回来。所以在原生预览通路（Tauri 资源协议 / fs 读取）
 * 出现之前，待发 chip 的缩略图只能是占位块：`pending` 为真时读取失败画**中性**
 * 占位（`title="发送后可预览"`），绝不画红色 ✗——那是预期内的事实，不是错误。
 * 只有**本该可读**的路径（服务端回显的 workspace 相对引用、工具结果路径）读失败
 * 才升级成失败标记。
 */
export function AttachmentThumb({
  path,
  threadId,
  call,
  pending = false,
}: {
  path: string;
  threadId: string | null;
  call: ImageReadCall;
  /** 这张 chip 是否还在待发列表（尚未发送）。见组件头部注释里的限制说明。 */
  pending?: boolean;
}) {
  const { url, error } = useImageData(path, { threadId, call });

  if (error && pending) {
    // 待发图片的绝对路径读不回来是预期内的（见顶部注释）：给中性占位块而不是
    // 失败标记，并给出明确标题，说明它发送后即可预览。布局与其它占位块一致。
    return (
      <span
        data-testid="attachment-thumb"
        title="发送后可预览"
        className="h-10 w-10 shrink-0 rounded border border-line bg-surface"
      />
    );
  }

  if (error) {
    // 路径本该可读却失败：这才是真错误。读不回来就不画破图，留一个小标记
    // （chip 上的文件名与移除按钮照旧可用）。
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
