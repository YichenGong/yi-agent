/**
 * iOS 配对页的相机扫码循环（S4）。
 *
 * 逻辑与 DOM 解耦：整个过程依赖三件事——拿视频流、抓一帧、解码一帧——都可由
 * `deps` 注入。于是「命中即停」「多帧才命中」「解码抛错继续」「无相机即报错」
 * 都能在 jsdom 里用假实现测。真实 `getUserMedia` 只能真机手验。
 */

import jsQR from "jsqr";

/** 默认逐帧间隔（毫秒）。 */
export const SCAN_INTERVAL_MS = 200;

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
 * 打开相机并逐帧解码，命中即调 `onResult` 并停止。返回 `stop()` 用于清理。
 * `getUserMedia` 失败时调 `onError` 且不启动循环（表单仍可手输）。
 *
 * 取流与 `play()` 都**不会**同步抛：任何失败一律走 `onError`，调用方（React effect）
 * 因此不会因为一个异常而连累整棵树。
 */
export function startCameraScan(
  video: HTMLVideoElement,
  onResult: (text: string) => void,
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
      // 丢弃，于是预览停在第一帧（黑屏）而用户得不到任何反馈。
      Promise.resolve(video.play()).catch((e) => {
        stop();
        onError(e);
      });
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
          stop();
          onResult(text);
        }
      }, intervalMs);
    })
    .catch((e) => {
      stop();
      onError(e);
    });

  return stop;
}
