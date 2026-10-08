/**
 * 附件的前端预检。
 *
 * 服务端是权威（它会拒绝并给出错误），这里只做**发送前**的就地提示，省掉一次
 * 注定失败的往返。上限与服务端的 `MAX_ATTACHMENT_BYTES` 保持一致
 * （`yi-agent-app-server/src/attachments.rs`）。
 */

export const MAX_ATTACHMENT_BYTES = 50 * 1024 * 1024;

/** 与 `read_document` 支持的格式一致。 */
export const ATTACHMENT_EXTENSIONS = [
  "pdf",
  "docx",
  "txt",
  "md",
  "csv",
  "html",
  "htm",
  "rtf",
] as const;

/** 尚未发送的附件：只有本地路径与文件名，服务端元数据要发送后才产生。 */
export interface PendingAttachment {
  path: string;
  name: string;
  size: number;
}

/** 路径的最后一段（兼容 `/` 与 `\`）。 */
export function fileNameOf(path: string): string {
  const parts = path.split(/[\\/]/);
  return parts[parts.length - 1] || path;
}

function extensionOf(path: string): string {
  const name = fileNameOf(path);
  const dot = name.lastIndexOf(".");
  return dot < 0 ? "" : name.slice(dot + 1).toLowerCase();
}

/** 返回不可发送的原因；可发送返回 null。 */
export function attachmentProblem(path: string, size: number): string | null {
  const ext = extensionOf(path);
  if (!(ATTACHMENT_EXTENSIONS as readonly string[]).includes(ext)) {
    return `不支持的文件类型：${ext || "（无扩展名）"}`;
  }
  if (size > MAX_ATTACHMENT_BYTES) {
    return "文件超过 50 MB 上限";
  }
  return null;
}
