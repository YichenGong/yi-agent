/**
 * iOS 配对页的相机扫码循环（S4）。
 *
 * 逻辑与 DOM 解耦：整个过程依赖三件事——拿视频流、抓一帧、解码一帧——都可由
 * `deps` 注入。于是「接受即停」「拒绝继续扫」「多帧才命中」「解码抛错继续」
 * 「无相机即报错」都能在 jsdom 里用假实现测。真实 `getUserMedia` 只能真机手验。
 */

import jsQR from "jsqr";

/** 默认逐帧间隔（毫秒）。 */
export const SCAN_INTERVAL_MS = 200;

/**
 * 扫描结果接收契约：`false` 表示这一帧不算数、继续扫描；`true` 或 `undefined`
 * 表示接受该结果并停止本次扫描。
 *
 * 刻意是同步签名：扫码循环在帧回调里直接判返回值，异步结果无法在这一帧决定
 * 是否继续，会把「已接受」与「还在扫」混在一起。
 */
export type ScanResultHandler = (text: string) => boolean | void;

export interface ScanDeps {
  /** 默认 `navigator.mediaDevices.getUserMedia`。 */
  getUserMedia?: (c: MediaStreamConstraints) => Promise<MediaStream>;
  /** 默认：把 video 当前帧画到 canvas 并取 `ImageData`。 */
  grabFrame?: (video: HTMLVideoElement) => ImageData | null;
  /** 默认：`jsQR` 包装。返回解码文本或 null。 */
  decode?: (data: ImageData) => string | null;
  intervalMs?: number;
}

/** 默认抓帧：canvas 尺寸随视频尺寸，取整帧像素。 */
function defaultGrabFrame(video: HTMLVideoElement): ImageData | null {
  const w = video.videoWidth;
  const h = video.videoHeight;
  if (!w || !h) return null;
  const canvas = document.createElement("canvas");
  canvas.width = w;
  canvas.height = h;
  const ctx = canvas.getContext("2d");
  if (!ctx) return null;
  ctx.drawImage(video, 0, 0, w, h);
  return ctx.getImageData(0, 0, w, h);
}

/**
 * 默认取流，**永不抛异常**（同步也不抛）。
 *
 * `MediaDevices` 在 WebIDL 里标了 `[SecureContext]`：上下文不安全时（iOS 的
 * WKWebView + 自定义 scheme）整个 `navigator.mediaDevices` 会**消失**，于是
 * `navigator.mediaDevices.getUserMedia` 是同步 TypeError，而不是 promise rejection
 * ——挂在 promise 链上的 `.catch` 接不住它。它又发生在 `QrScanner` 的 `useEffect`
 * 里，会冒泡成 React 的整树卸载（黑屏）。故这里把它压成 rejected promise，
 * 统一交给 `onError` 处理，让失败退化为「可手输」而不是崩溃。
 */
function defaultGetUserMedia(c: MediaStreamConstraints): Promise<MediaStream> {
  try {
    const md = navigator.mediaDevices;
    if (!md || typeof md.getUserMedia !== "function") {
      return Promise.reject(new Error("mediaDevices unavailable (insecure context?)"));
    }
    return md.getUserMedia(c);
  } catch (e) {
    return Promise.reject(e);
  }
}

/** 默认解码：`jsQR`（npm `jsqr`，导入名 `jsQR`）。 */
function defaultDecode(data: ImageData): string | null {
  const r = jsQR(data.data, data.width, data.height, { inversionAttempts: "dontInvert" });
  return r?.data ?? null;
}

/**
 * 打开相机并逐帧解码，调 `onResult` 接收结果：返回 `false` 则继续扫描，返回
 * `true`/`undefined` 则接受并停止。返回 `stop()` 用于清理（幂等）。
 * `getUserMedia` 失败时调 `onError` 且不启动循环（表单仍可手输）。
 *
 * 取流与 `play()` 都**不会**同步抛：停机前的失败一律走 `onError`，调用方（React
 * effect）因此不会因为一个异常而连累整棵树。停机（含已接受结果）之后到达的
 * 取流/播放 rejection 静默丢弃，不再打扰已经离开的调用方。
 */
export function startCameraScan(
  video: HTMLVideoElement,
  onResult: ScanResultHandler,
  onError: (e: unknown) => void,
  deps: ScanDeps = {},
): () => void {
  const intervalMs = deps.intervalMs ?? SCAN_INTERVAL_MS;
  const grabFrame = deps.grabFrame ?? defaultGrabFrame;
  const decode = deps.decode ?? defaultDecode;
  const getUserMedia = deps.getUserMedia ?? defaultGetUserMedia;

  let timer: ReturnType<typeof setInterval> | null = null;
  let stopped = false;
  let stream: MediaStream | null = null;

  const stop = () => {
    if (stopped) return;
    stopped = true;
    if (timer !== null) {
      clearInterval(timer);
      timer = null;
    }
    stream?.getTracks().forEach((t) => t.stop());
    // 只在 `srcObject` 仍属于本实例时清空：晚到的 `stop()` 不能抹掉别的实例接上的流。
    if (stream && video.srcObject === stream) video.srcObject = null;
  };

  /** 唯一失败出口：已停则静默丢弃，否则先停机再上报**恰好一次**。 */
  const fail = (error: unknown) => {
    if (stopped) return;
    stop();
    onError(error);
  };

  /** 接入取流：注入的实现可能同步抛，故连同调用一起包住，同步异常也归到 onError。 */
  const requestStream = (): Promise<MediaStream> => {
    try {
      return Promise.resolve(getUserMedia({ video: { facingMode: "environment" } }));
    } catch (e) {
      return Promise.reject(e);
    }
  };

  requestStream()
    .then((s) => {
      if (stopped) {
        s.getTracks().forEach((t) => t.stop());
        return;
      }
      stream = s;
      video.srcObject = s;
      // `play()` 的失败也要上报：iOS 上元数据未就绪时它可能 reject，此前被 `void`
      // 丢弃，于是预览停在第一帧（黑屏）而用户得不到任何反馈。同步 throw 留给
      // 外层 `.catch(fail)` 收敛，`fail` 保证停止后不再重复上报。
      Promise.resolve(video.play()).catch(fail);
      timer = setInterval(() => {
        if (stopped) return;
        const frame = grabFrame(video);
        if (!frame) return;
        let text: string | null = null;
        try {
          text = decode(frame);
        } catch {
          return; // 单帧解码失败不是致命错误，继续。
        }
        if (text) {
          try {
            // `false` = 这一帧不算数，继续扫；`true`/`undefined` = 接受即停。
            if (onResult(text) !== false) stop();
          } catch (error) {
            // 结果处理方同步抛错：收敛为一次可见失败，而不是从 interval 逃逸。
            // 若它其实已经自己停过（`stopped` 为真），`fail` 会静默——此时没有
            // 还在扫的实例需要收尾，也就无处上报；这是刻意的，不是漏报。
            fail(error);
          }
        }
      }, intervalMs);
    })
    .catch(fail);

  return stop;
}
