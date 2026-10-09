import { AttachmentThumb } from "./AttachmentThumb";
import { fileNameOf } from "../lib/attachmentLimits";
import type { PendingAttachment } from "../lib/attachmentLimits";
import type { ImageReadCall } from "../lib/useImageData";

/**
 * 一个待发送附件的字节数转成人读的大小。
 *
 * 只用于展示：真正的上限判断在 `attachmentProblem`（服务端才是权威）。
 */
function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

/**
 * 未注入 `call` 时的替身（测试/嵌入场景）。**必须是模块级常量**：若在这里写内联
 * 箭头，chip 每次渲染都会换新引用，缩略图的读取 effect 就会反复重跑。
 */
const NO_READ: ImageReadCall = () => Promise.reject(new Error("image/read unavailable"));

/**
 * 输入框上方待发送的附件行：每个附件一个 chip，可单独移除。
 *
 * 只有本地路径与文件名（服务端元数据要发送后才产生），所以这里显示的就是
 * 用户挑中的那个文件，移除即从待发列表里拿掉——不触碰磁盘。
 *
 * 图片额外带一张缩略图（文档不带）：图片没有可读的"名字"以外的信息，看到图才知道
 * 挑对了没有。字节由 `AttachmentThumb` 经 `image/read` 另取。
 */
export function AttachmentChips({
  attachments,
  onRemove,
  threadId = null,
  call,
}: {
  attachments: PendingAttachment[];
  onRemove: (path: string) => void;
  /** 当前会话 id，透传给缩略图读取；`null`（无会话）时不读取。 */
  threadId?: string | null;
  /**
   * 宿主注入的图片读取接缝（`App` 的 `imageCall`）。**必须稳定引用**：它进
   * `useImageData` 的依赖数组，每次渲染换新身份会让每张缩略图重新分片拉取。
   * 未注入（测试/嵌入场景）时与 `ChatView` 的 `NO_CALL` 同形：缩略图退化成读取
   * 失败的标记，文件名与移除按钮照常可用。
   */
  call?: ImageReadCall;
}) {
  if (attachments.length === 0) return null;
  return (
    <div className="flex flex-wrap gap-1 px-2 pt-2" data-testid="attachment-chips">
      {attachments.map((a) => {
        // `name` 来自文件选择器；理论上可能为空（父级给的路径是唯一真相），
        // 那时退回路径的最后一段，而不是留一个没有标题的 chip。
        const label = a.name || fileNameOf(a.path);
        return (
          <span
            key={a.path}
            className="flex items-center gap-1 rounded border border-line bg-raised px-2 py-0.5 text-xs text-fg-muted"
          >
            {/* 只有图片有缩略图：文档走 `read_document` 清单，本地不必（也不该）
                去读它的字节。`NO_READ` 让未注入 `call` 时也能安全渲染。 */}
            {a.kind === "image" ? (
              <AttachmentThumb path={a.path} threadId={threadId} call={call ?? NO_READ} />
            ) : null}
            <span className="max-w-[16rem] truncate" title={a.path}>
              {label}
            </span>
            {/* `size: 0` 意为「未知」（桌面端 dialog 只给路径），不是 0 字节：
                此时整段省略，绝不渲染成 "0 B" 假装知道大小。 */}
            {a.size > 0 ? <span className="text-fg-subtle">{formatSize(a.size)}</span> : null}
            <button
              type="button"
              // 逐附件定位：多个 chip 时必须说清移除的是哪一个。
              aria-label={`移除 ${label}`}
              className="text-fg-subtle hover:text-red-400"
              onClick={() => onRemove(a.path)}
            >
              ×
            </button>
          </span>
        );
      })}
    </div>
  );
}
