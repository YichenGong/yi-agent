import { useEffect, useState } from "react";
import { formatError } from "./errorMessage";

/**
 * 宿主注入的图片读取接缝。
 *
 * 非泛型签名（与 `ModelPickerCall` 同形）：宿主传进来的稳定函数与测试里手写的普通
 * 函数都能直接赋值，运行时形状与 `RpcClient.request` 一致。
 */
export type ImageReadCall = (method: string, params: unknown) => Promise<unknown>;

/** `image/read` 的分片响应（见 app-server 的 `image/read`）。 */
interface ImageChunk {
  /** 原始文件字节的一段，base64 编码。 */
  data: string;
  /** 下一片的 offset；`null` = 已是最后一片。 */
  nextOffset: number | null;
  mediaType: string;
  /** **存储文件**的字节长度，不是本片的长度；仅当大小提示用。 */
  size: number;
}

/**
 * 对象 URL 的 LRU 上限。
 *
 * 一条长会话里图片可能很多，而每个 Blob URL 都会把整张图的字节钉在内存里直到
 * `revokeObjectURL`。上限取 60：足以覆盖「刚滚过去还想往上翻一眼」的窗口，又不至于
 * 让几百张截图把标签页拖垮。淘汰即释放。
 */
export const IMAGE_CACHE_LIMIT = 60;

/**
 * path → 对象 URL。用 `Map` 的插入顺序当 LRU：命中时 delete + set 把条目挪到队尾，
 * 淘汰从队首（最久未用）取。
 */
const cache = new Map<string, string>();

/**
 * 同一 path 的**在途**读取。
 *
 * 两个组件可能在同一帧里为同一张图挂载（例如同一条消息里的两张相同的 ref）：只看
 * 缓存会双双 miss 并发起重复分片请求。这里把并发折成一次，先到的发起、后到的汇合。
 */
const inflight = new Map<string, Promise<string>>();

/** 命中则刷新 LRU 次序并返回 URL，未命中返回 `null`。 */
function touchCache(path: string): string | null {
  const url = cache.get(path);
  if (url === undefined) return null;
  cache.delete(path);
  cache.set(path, url);
  return url;
}

/** 存入缓存；超出上限时淘汰并释放最久未用的对象 URL。 */
function storeCache(path: string, url: string): void {
  cache.set(path, url);
  while (cache.size > IMAGE_CACHE_LIMIT) {
    const oldest = cache.entries().next().value as [string, string];
    cache.delete(oldest[0]);
    URL.revokeObjectURL(oldest[1]);
  }
}

/**
 * base64 → 字节。
 *
 * 用 `atob` 拿到「每字符一字节」的二进制字符串，再逐字符搬运。刻意不用
 * `String.fromCharCode(...bin)` 展开：一片最大 512 KiB，展开成实参会爆栈。
 */
function decodeBase64(data: string): Uint8Array {
  const bin = atob(data);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

/** 逐片拉取到 `nextOffset === null`，把字节按序拼成一张 Blob。 */
async function fetchImageBlob(
  call: ImageReadCall,
  threadId: string,
  path: string,
): Promise<Blob> {
  const chunks: Uint8Array[] = [];
  let total = 0;
  let mediaType = "";
  let offset: number | null = 0;

  while (offset !== null) {
    const raw = await call("image/read", { threadId, path, offset });
    if (!raw || typeof raw !== "object") {
      throw new Error("invalid image/read response");
    }
    const chunk = raw as Partial<ImageChunk>;
    const bytes = decodeBase64(chunk.data ?? "");
    chunks.push(bytes);
    total += bytes.length;
    if (chunk.mediaType) mediaType = chunk.mediaType;
    // 缺省/`undefined` 一律当终止，避免坏响应把循环拖成死转。
    const next = chunk.nextOffset ?? null;
    // 防御：offset 必须严格前进。坏服务端若原地返回同一个 offset，这里当作终止，
    // 否则这个循环会永远转下去（宁可截断，也不能把标签页挂死）。
    if (next !== null && next <= offset) break;
    offset = next;
  }

  const merged = new Uint8Array(total);
  let at = 0;
  for (const c of chunks) {
    merged.set(c, at);
    at += c.length;
  }
  // 字节数以**解码出的长度**为准：响应里的 `size` 是存储文件的长度，对本地
  // 追加/截断过的文件可能对不上，不能拿它裁剪。
  return new Blob([merged], { type: mediaType });
}

/** 取（或发起）一次读取，返回可显示的对象 URL。 */
function load(call: ImageReadCall, threadId: string, path: string): Promise<string> {
  const pending = inflight.get(path);
  if (pending) return pending;
  const job = (async () => {
    const url = URL.createObjectURL(await fetchImageBlob(call, threadId, path));
    storeCache(path, url);
    return url;
  })().finally(() => {
    // 失败也从在途表里摘掉：下一次挂载可以重试，而不是永久汇合到一个失败的 Promise。
    inflight.delete(path);
  });
  inflight.set(path, job);
  return job;
}

/**
 * 按需读取一张图片，返回可直接给 `<img src>` 的对象 URL。
 *
 * 内容不进协议：`ImageRef` 只有定位元数据，字节由 `image/read` 分片取。分片循环到
 * `nextOffset === null`，拼成 Blob 后 `createObjectURL`。
 *
 * 缓存键是 `path`（同一工作区里哈希文件名唯一），跨组件共享：第二张相同的 `ref`
 * 命中缓存，**不发请求**。淘汰时释放对象 URL。
 *
 * `threadId` 为 `null` 或 `path` 为空时不读取（无会话/无引用），返回空状态而不是报错。
 *
 * 卸载后不再 setState；`call` 被拒时把可读的消息放进 `error`。`call` 必须是稳定
 * 引用（见 `App` 的 `imageCall`），否则每次渲染都会重新拉取。
 */
export function useImageData(
  path: string,
  opts: { threadId: string | null; call: ImageReadCall },
): { url: string | null; error: string | null } {
  const { threadId, call } = opts;
  const [state, setState] = useState<{ url: string | null; error: string | null }>(
    () => {
      if (!threadId || !path) return { url: null, error: null };
      // 命中缓存时**首帧**就有 URL：既不闪一下空白，也不触发读取 effect。
      return { url: touchCache(path), error: null };
    },
  );

  useEffect(() => {
    if (!threadId || !path) {
      // 清掉上一个 path 的结果（正常情况下一个 <img> 固定一个 path，这里只为防御）。
      setState((prev) =>
        prev.url === null && prev.error === null ? prev : { url: null, error: null },
      );
      return;
    }

    const cached = touchCache(path);
    if (cached !== null) {
      // 返回同一个对象即让 React 跳过重渲染；不能只是「值相等」，对象身份不等。
      setState((prev) => (prev.url === cached ? prev : { url: cached, error: null }));
      return;
    }

    let alive = true;
    void load(call, threadId, path).then(
      (url) => {
        if (alive) setState({ url, error: null });
      },
      (e: unknown) => {
        if (alive) setState({ url: null, error: formatError(e) });
      },
    );
    return () => {
      alive = false;
    };
  }, [path, threadId, call]);

  return state;
}
