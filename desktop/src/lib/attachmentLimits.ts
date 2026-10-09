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

/**
 * 与 `view_image` 支持的格式一致。
 *
 * 与 `ATTACHMENT_EXTENSIONS` **不重叠**：两个白名单互斥，故路径的扩展名足以
 * 判定这份文件是文档还是图片。
 */
export const IMAGE_EXTENSIONS = ["png", "jpg", "jpeg", "gif", "webp"] as const;

/** 与服务端的 `MAX_IMAGE_BYTES` 保持一致（`yi-agent-tools/src/image_prep.rs`）。 */
export const MAX_IMAGE_BYTES = 20 * 1024 * 1024;

/** 尚未发送的附件：只有本地路径与文件名，服务端元数据要发送后才产生。 */
export interface PendingAttachment {
  path: string;
  name: string;
  size: number;
  /**
   * 这份待发附件是文档还是图片。发送时二者的协议形态不同：文档走清单
   * （`{type:"attachment", path}`，agent 用 `read_document` 按需读），图片走
   * 内容块（`{type:"image", path}`，服务端摄取成模型直接可见的图片）。
   */
  kind: "document" | "image";
  /**
   * 已上传完成、落在服务端工作区里的图片句柄（远端/iOS 路径）。
   *
   * 有无它决定发送时的协议形态：有则送 `{type:"uploaded_image", uploadId}`（字节
   * 早已在服务端落盘，句柄是唯一引用，此时 `path` 只供本地渲染）；无则送
   * `{type:"image", path}`（桌面端：服务端自己按路径读盘）。
   */
  uploadId?: string;
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

/**
 * 图片的**大小**预检（不看扩展名）。
 *
 * 远端上传路径单独用它：HEIC/HEIF 必须放行到 `uploadImage` 的转码那一步（扩展名
 * 白名单会把它当「不支持的图片类型」拦下），但 20 MB 上限对所有图片一律先查。
 */
export function imageSizeProblem(size: number): string | null {
  return size > MAX_IMAGE_BYTES ? "图片超过 20 MB 上限" : null;
}

/**
 * 图片的预检：返回不可发送的原因；可发送返回 null。
 *
 * 与 `attachmentProblem` 分开：图片不进 `read_document` 的清单，走的是内容块，
 * 类型白名单与上限都不同（20 MB vs 50 MB）。
 */
export function imageAttachmentProblem(path: string, size: number): string | null {
  const ext = extensionOf(path);
  if (!(IMAGE_EXTENSIONS as readonly string[]).includes(ext)) {
    return `不支持的图片类型：${ext || "（无扩展名）"}`;
  }
  return imageSizeProblem(size);
}

/**
 * 由路径的扩展名判定待发附件的类别（两个白名单不重叠）。
 *
 * 额外白名单（文档或图片）一律归为 `"document"`：随后的预检会就地报错并丢弃，
 * 它进不了待发列表，故这里的归类只需在**能通过预检**的路径上是准确的。
 */
export function pendingKindFromPath(path: string): "document" | "image" {
  return (IMAGE_EXTENSIONS as readonly string[]).includes(extensionOf(path))
    ? "image"
    : "document";
}
