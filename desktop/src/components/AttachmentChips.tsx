import { fileNameOf } from "../lib/attachmentLimits";
import type { PendingAttachment } from "../lib/attachmentLimits";

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
 * 输入框上方待发送的附件行：每个附件一个 chip，可单独移除。
 *
 * 只有本地路径与文件名（服务端元数据要发送后才产生），所以这里显示的就是
 * 用户挑中的那个文件，移除即从待发列表里拿掉——不触碰磁盘。
 */
export function AttachmentChips({
  attachments,
  onRemove,
}: {
  attachments: PendingAttachment[];
  onRemove: (path: string) => void;
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
