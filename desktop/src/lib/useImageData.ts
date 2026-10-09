import { useEffect, useRef, useState } from "react";
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
 * 缓存键 → 对象 URL。用 `Map` 的插入顺序当 LRU：命中时 delete + set 把条目挪到
 * 队尾，淘汰从队首（最久未用）取。
 *
 * 键是 `threadId:path`，两者缺一不可。path 单独不够用——工具结果的图片引用带的是
 * 模型 `view_image` 的原样参数，即 `docs/diagram.png` 这样的**工作区相对**路径
 * （用户发送的引用形如 `.yi-agent/attachments/<tid>/…` 才是 thread 内的）。两条
 * cwd 不同的 thread 完全可能各自看过一个同名相对路径，只按 path 做键就会让后来的
 * 那条直接命中前一条的字节，静默显示成别人的图。thread 前缀让「同一 thread、
 * 同一路径」照旧共享，「不同 thread」各读各的。
 */
const cache = new Map<string, string>();

/**
 * 同一缓存键的**在途**读取。
 *
 * 两个组件可能在同一帧里为同一张图挂载（例如同一条消息里的两张相同的 ref）：只看
 * 缓存会双双 miss 并发起重复分片请求。这里把并发折成一次，先到的发起、后到的汇合。
 */
const inflight = new Map<string, Promise<string>>();

/**
 * 对象 URL → 当前仍持有它的**已挂载**消费者数量。
 *
 * 淘汰一个还有人在显示的对象 URL 会让 `<img>` 变死图，且因为 state 已经等于该
 * URL、effect 依赖又没变，组件不会自愈。所以淘汰前先看这里：有活消费者就推迟
 * `revokeObjectURL`，等最后一个消费者卸载再放。
 *
 * 按 **URL**（而非 path）计数：每次 `createObjectURL` 都返回唯一 URL，同一个 path
 * 被重新读回时是新 URL，于是「重新读回」天然不会误伤旧 URL、也不会重复回收。
 *
 * 计数**只在 effect 里加减**（它和清理函数成对）：`useState` 初始化器在 StrictMode
 * 下会被重复调用，在里面计数会重复累加、导致最后一个消费者卸载后也回收不掉。
 */
const liveRefs = new Map<string, number>();

/**
 * 已淘汰（出了 LRU）但仍有人在显示、暂不能回收的对象 URL。
 *
 * 最后一个持有它的消费者卸载时回收并清掉；若中途该 path 被重新读回也只是新 URL
 * 进缓存，旧 URL 仍只由「最后一个旧持有者卸载」来回收——不重复、不悬空。
 */
const deferredRevoke = new Set<string>();

/** 缓存键：`threadId:path`（见 `cache` 上的说明）。 */
function cacheKey(threadId: string, path: string): string {
  return `${threadId}:${path}`;
}

/** 命中则刷新 LRU 次序并返回 URL，未命中返回 `null`。 */
function touchCache(key: string): string | null {
  const url = cache.get(key);
  if (url === undefined) return null;
  cache.delete(key);
  cache.set(key, url);
  return url;
}

/** 记一个消费者开始持有 `url`。 */
function acquireRef(url: string): void {
  liveRefs.set(url, (liveRefs.get(url) ?? 0) + 1);
}

/** 记一个消费者不再持有 `url`；这是最后一个持有者且该 URL 已被淘汰时，回收它。 */
function releaseRef(url: string): void {
  const left = (liveRefs.get(url) ?? 0) - 1;
  if (left > 0) {
    liveRefs.set(url, left);
    return;
  }
  liveRefs.delete(url);
  if (deferredRevoke.has(url)) {
    deferredRevoke.delete(url);
    URL.revokeObjectURL(url);
  }
}

/** 存入缓存；超出上限时淘汰最久未用的条目，无人显示者当场释放、有人在看者推迟。 */
function storeCache(key: string, url: string): void {
  cache.set(key, url);
  while (cache.size > IMAGE_CACHE_LIMIT) {
    const oldest = cache.entries().next().value as [string, string];
    cache.delete(oldest[0]);
    if ((liveRefs.get(oldest[1]) ?? 0) > 0) {
      // 还有 <img> 在显示这张图：推迟回收，等最后一个消费者卸载。
      deferredRevoke.add(oldest[1]);
    } else {
      URL.revokeObjectURL(oldest[1]);
    }
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
  const key = cacheKey(threadId, path);
  const pending = inflight.get(key);
  if (pending) return pending;
  const job = (async () => {
    const url = URL.createObjectURL(await fetchImageBlob(call, threadId, path));
    storeCache(key, url);
    return url;
  })().finally(() => {
    // 失败也从在途表里摘掉：下一次挂载可以重试，而不是永久汇合到一个失败的 Promise。
    inflight.delete(key);
  });
  inflight.set(key, job);
  return job;
}

/**
 * 按需读取一张图片，返回可直接给 `<img src>` 的对象 URL。
 *
 * 内容不进协议：`ImageRef` 只有定位元数据，字节由 `image/read` 分片取。分片循环到
 * `nextOffset === null`，拼成 Blob 后 `createObjectURL`。
 *
 * 缓存键是 `threadId:path`——**不是** path 单独。工具结果的图片引用带的是模型
 * `view_image` 的原样参数，`docs/diagram.png` 这类工作区相对路径在两条 cwd 不同的
 * thread 里可以重名，只看 path 就会让后一条静默显示前一条的字节（跨会话串图）。
 * 同一 thread 内则照旧共享：第二张相同的 `ref` 命中缓存，**不发请求**。
 * 淘汰时释放对象 URL；但若仍有已挂载消费者在显示该 URL，则推迟到最后一个消费者
 * 卸载再释放（见 `deferredRevoke`）。
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
  // 本消费者**当前持有计数的那份 URL**（未持有为 `null`）。所有计数加减都在 effect
  // 里做：effect 与清理函数成对，StrictMode 下也不会重复累加。
  const heldRef = useRef<string | null>(null);
  const [state, setState] = useState<{ url: string | null; error: string | null }>(
    () => {
      if (!threadId || !path) return { url: null, error: null };
      // 命中缓存时**首帧**就有 URL：既不闪一下空白，也不触发读取 effect。
      // 这里**不**计数（初始化器可能被 StrictMode 重复调用）；effect 会为这份 URL
      // 记账，消费者照样计入引用。
      return { url: touchCache(cacheKey(threadId, path)), error: null };
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

    // 归还本消费者占用的那份计数（卸载、换 URL、依赖变化时都走这里，幂等）。
    const release = () => {
      const held = heldRef.current;
      if (held === null) return;
      heldRef.current = null;
      releaseRef(held);
    };
    // 记下本消费者现在持有 `url` 的计数；换 URL 时先把旧的还掉，避免漏计/多计。
    const hold = (url: string) => {
      if (heldRef.current === url) return;
      release();
      acquireRef(url);
      heldRef.current = url;
    };

    const cached = touchCache(cacheKey(threadId, path));
    if (cached !== null) {
      // 命中缓存也要计数：否则淘汰会收回一张正在显示的图。初始化器很可能已把同一
      // 份 URL 放进 state（首帧即显示），这里再 `hold` 它，计数与显示一致。
      hold(cached);
      // 返回同一个对象即让 React 跳过重渲染；不能只是「值相等」，对象身份不等。
      setState((prev) => (prev.url === cached ? prev : { url: cached, error: null }));
      return release;
    }

    let alive = true;
    void load(call, threadId, path).then(
      (url) => {
        if (!alive) return;
        // 读回即持有：这份 URL 归本次挂载，卸载时归还，使淘汰能推迟到那时。
        hold(url);
        setState({ url, error: null });
      },
      (e: unknown) => {
        if (alive) setState({ url: null, error: formatError(e) });
      },
    );
    return () => {
      alive = false;
      release();
    };
  }, [path, threadId, call]);

  return state;
}
