/**
 * 远端（iOS）图片分片上传客户端。
 *
 * 桌面端把图片**按路径**交给服务端（`{type:"image", path}`，服务端自己读盘）；
 * iOS 没有共享文件系统，只能把字节送上去：`image/upload/begin` 报上大小开一条
 * 会话，随后按 `index` 递增送块（base64，服务端解码），最后 `commit` 收拢落盘。
 *
 * 这里只做「切块 + 按序送 + 收尾」，不含任何 UI：调用方（`App`）拿 `{uploadId, path}`
 * 把它记进待发附件（`path` 是工作区相对路径，故 chip 的缩略图能经 `image/read` 读回）。
 */

import { MAX_IMAGE_BYTES } from "./attachmentLimits";

/** 与 app-server 的 `image_upload::UPLOAD_CHUNK_BYTES` 保持一致（512 KiB）。 */
export const UPLOAD_CHUNK_BYTES = 512 * 1024;

/**
 * 宿主注入的 RPC 接缝，与 `ImageReadCall`/`modelCall` 同形的普通函数（本仓库没有
 * RPC context，一律由宿主按 props/参数注入；见 `App` 的 `imageCall`）。
 */
export type UploadCall = (method: string, params: unknown) => Promise<unknown>;

/** 上传成功的句柄：`uploadId` 送模型时引用，`path` 供本地渲染（缩略图）。 */
export interface UploadedImage {
  uploadId: string;
  /**
   * 落盘后的**工作区相对**路径（如 `.yi-agent/attachments/<tid>/xxxxxxxx-照片.jpg`）。
   * `image/read` 以会话 cwd 为根，故这个路径可以直接读回来画缩略图——这正是本地
   * 文件对话框给的绝对路径做不到的（见 `AttachmentThumb`）。
   */
  path: string;
}

/** HEIC/HEIF 的 MIME。iOS 相册默认交付这两种，而服务端的 `image` crate 不认。 */
const HEIC_MIME = ["image/heic", "image/heif", "image/heic-sequence", "image/heif-sequence"];
/** 同上，按扩展名兜底：iOS 有时给空 `type`。 */
const HEIC_EXT = ["heic", "heif"];

/** JPEG 转码质量。0.9 是截图/照片在体积与观感之间的常用折中。 */
const HEIC_JPEG_QUALITY = 0.9;

/** 文件名的扩展名（小写，不含点）。 */
function extensionOf(name: string): string {
  const dot = name.lastIndexOf(".");
  return dot < 0 ? "" : name.slice(dot + 1).toLowerCase();
}

/** 这份文件是不是 HEIC/HEIF（MIME 优先，其次看扩展名）。 */
export function isHeic(file: File): boolean {
  return HEIC_MIME.includes(file.type.toLowerCase()) || HEIC_EXT.includes(extensionOf(file.name));
}

/** 把 HEIC 的名字换成 `.jpg`（转码后是 JPEG，名字也要跟着对）。 */
function jpegName(name: string): string {
  const dot = name.lastIndexOf(".");
  return `${dot <= 0 ? name : name.slice(0, dot)}.jpg`;
}

/**
 * 读整个文件的字节。
 *
 * 用 `FileReader` 而不是 `Blob.arrayBuffer()`：后者在 iOS 的 WKWebView 上支持面更窄
 * （Tauri 的 iOS 壳就是 WKWebView），FileReader 是各版本都在的那条路。
 */
function readBytes(file: File): Promise<Uint8Array> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onerror = () => reject(reader.error ?? new Error(`无法读取文件：${file.name}`));
    reader.onload = () => {
      const result = reader.result;
      resolve(result instanceof ArrayBuffer ? new Uint8Array(result) : new Uint8Array(0));
    };
    reader.readAsArrayBuffer(file);
  });
}

/**
 * 把一段字节 base64 编码。
 *
 * 按 4096 字节的窗口搬运：`String.fromCharCode(...bytes)` 会把每个字节摊成一个
 * 实参，一块 512 KiB 的片必然爆栈（引擎对实参个数有上限），所以切片拼接。
 */
function bytesToBase64(bytes: Uint8Array): string {
  const WINDOW = 4096;
  let bin = "";
  for (let i = 0; i < bytes.length; i += WINDOW) {
    bin += String.fromCharCode(...bytes.subarray(i, i + WINDOW));
  }
  return btoa(bin);
}

/** 转码失败时的统一说法：说清「为什么不行」和「怎么办」，不要静默上传 HEIC。 */
function heicFailure(file: File): Error {
  return new Error(
    `无法转码 HEIC 图片「${file.name}」：当前设备不能在浏览器里解码 HEIC。` +
      `请在系统相册里把它导出为 JPEG 后重试。`,
  );
}

/**
 * 转码后仍超过 20 MB 上限时的拒绝理由。
 *
 * 调用方按**原始**文件大小预检（`imageSizeProblem`），转码只在 HEIC 上发生，而
 * JPEG 通常比同图 HEIC 大，**转码后**超限完全可能。服务端的 `begin` 只看**声明**
 * 大小，若客户端把转码后的字节数报上去，恰好卡在「声明 == 实际」这一条上被放行，
 * 于是这张图会一路 begin+commit 成功，直到 `turn/start` 的 `prepare_image_file`
 * 才因文件本身超限而炸；那会毒死整轮，而 chip 还留着，用户每次重试都同样失败。
 * 所以必须在开口（`begin`）之前就地拒绝。桌面 `{type:"image", path}` 那条路一直
 * 有服务端这道大小闸，这里补上对称的一道。
 */
function transcodedTooLarge(file: File): Error {
  const limit = Math.round(MAX_IMAGE_BYTES / (1024 * 1024));
  return new Error(
    `转码后的图片「${file.name}」超过 ${limit} MB 上限（约 ${(file.size / (1024 * 1024)).toFixed(1)} MB）；` +
      `请先把它裁小或降低分辨率后重试。`,
  );
}

/**
 * HEIC/HEIF → JPEG。
 *
 * 服务端的 `image` crate 不认 HEIC（`guess_format` 直接失败），所以**必须**在客户端
 * 先转码，否则这张图到了服务端只会被拒。路径是 webview 自带的能力：
 * `createImageBitmap` 解码 + `canvas.toBlob("image/jpeg", 0.9)` 编码。
 *
 * 任何一步不可用（老引擎没有 `createImageBitmap`、解码失败、编码回 `null`）都抛
 * 明确错误——绝不把 HEIC 原样传上去假装成功。
 */
async function transcodeHeicToJpeg(file: File): Promise<File> {
  const decode = globalThis.createImageBitmap;
  if (typeof decode !== "function") throw heicFailure(file);

  let bitmap: ImageBitmap;
  try {
    bitmap = await decode(file);
  } catch {
    // 解码失败的原因很多（引擎不支持该容器、文件损坏），对用户都是同一件事。
    throw heicFailure(file);
  }

  try {
    const canvas = document.createElement("canvas");
    canvas.width = bitmap.width;
    canvas.height = bitmap.height;
    const ctx = canvas.getContext("2d");
    if (!ctx) throw heicFailure(file);
    ctx.drawImage(bitmap, 0, 0);
    const blob = await new Promise<Blob | null>((resolve) =>
      canvas.toBlob(resolve, "image/jpeg", HEIC_JPEG_QUALITY),
    );
    if (!blob) throw heicFailure(file);
    // 名字与 MIME 一起换成 JPEG：`begin` 的 name/mime 会决定服务端落盘名与回显的
    // mediaType，留着 `.HEIC` 只会让后续每一处都自相矛盾。
    return new File([blob], jpegName(file.name), { type: "image/jpeg" });
  } finally {
    // 位图可能是一整张图的像素，尽早释放（Firefox/Safari 不靠 GC 及时回收）。
    bitmap.close?.();
  }
}

/** `begin` 的响应里取 `uploadId`；形状不对就明说，别拿着 `undefined` 往下走。 */
function readUploadId(raw: unknown): string {
  const uploadId = (raw as { uploadId?: unknown } | null)?.uploadId;
  if (typeof uploadId !== "string" || uploadId.length === 0) {
    throw new Error("image/upload/begin 的响应缺少 uploadId");
  }
  return uploadId;
}

/** `commit` 的响应里取落盘路径。 */
function readCommittedPath(raw: unknown): string {
  const path = (raw as { path?: unknown } | null)?.path;
  if (typeof path !== "string" || path.length === 0) {
    throw new Error("image/upload/commit 的响应缺少 path");
  }
  return path;
}

/**
 * 把一个 `File` 分片上传，返回 `{uploadId, path}`。
 *
 * 序列：`begin` 一次（带**最终**字节数——HEIC 先转码，报的必须是转码后的大小）→
 * 按 `UPLOAD_CHUNK_BYTES` 切片、`chunk` 逐块（index 从 0 递增）→ `commit` 一次。
 *
 * `begin` 之后的任何失败都先尽力 `abort` 再原样抛出：不 abort 的话那条会话会占着
 * 该 thread 的未 commit 名额、并留一个 staging 文件直到 TTL（10 分钟）。`abort`
 * 本身失败不掩盖真正的错误（它幂等，服务端对未知 id 也回成功）。
 */
export async function uploadImage(
  call: UploadCall,
  threadId: string,
  file: File,
): Promise<UploadedImage> {
  // 转码在 `begin` **之前**：报给服务端的大小必须与实际要送的字节一致（累计字节
  // 不得超过声明大小是服务端的硬校验），所以先定下最终形态再开口。
  const upload = isHeic(file) ? await transcodeHeicToJpeg(file) : file;
  const bytes = await readBytes(upload);

  // 转码**之后**再查一次大小：报的是 `bytes.length`，若它超过 20 MB，服务端的
  // `begin`（比的是声明值与上限，二者此刻相等，会放行）与 `turn/start` 的
  // `prepare_image_file`（比的是落盘文件与上限，必拒）会在这里分岔——上传白忙一场，
  // 还毒死整轮。就地拒绝，一个请求都不发。非 HEIC 的图由调用方的
  // `imageSizeProblem` 预检（原始大小）拦住，走不到这里，故这条分支只对转码结果生效。
  if (upload !== file && bytes.length > MAX_IMAGE_BYTES) throw transcodedTooLarge(upload);

  const uploadId = readUploadId(
    await call("image/upload/begin", {
      threadId,
      name: upload.name,
      mime: upload.type,
      size: bytes.length,
    }),
  );

  try {
    for (let index = 0, offset = 0; offset < bytes.length; index += 1, offset += UPLOAD_CHUNK_BYTES) {
      const slice = bytes.subarray(offset, offset + UPLOAD_CHUNK_BYTES);
      await call("image/upload/chunk", { uploadId, index, data: bytesToBase64(slice) });
    }
    const committed = await call("image/upload/commit", { uploadId });
    return { uploadId, path: readCommittedPath(committed) };
  } catch (e) {
    try {
      await call("image/upload/abort", { uploadId });
    } catch {
      // 尽力而为：收尾失败不能把真正的失败盖掉。
    }
    throw e;
  }
}
