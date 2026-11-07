import { useEffect, useRef } from "react";
import {
  startCameraScan,
  type ScanDeps,
  type ScanResultHandler,
} from "../lib/qrScanner";

/** 全屏相机预览 + 扫描循环的薄 UI。逻辑全在 `lib/qrScanner`。 */
export interface QrScannerProps {
  /** 同一帧的接收契约：`false` 继续扫，`true`/`undefined` 接受即停。 */
  onText: ScanResultHandler;
  onError: (e: unknown) => void;
  /** 注入接缝（测试用）；生产省略。 */
  deps?: ScanDeps;
}

export function QrScanner({ onText, onError, deps }: QrScannerProps) {
  const videoRef = useRef<HTMLVideoElement | null>(null);
  // 回调只从 ref 读：父组件每次渲染都会新建闭包，若让它们进依赖数组，effect
  // 会在每次渲染后 `stop()` 再重新取流——相机会被反复开关（用户看到黑屏/闪断）。
  const latest = useRef({ onText, onError });
  latest.current = { onText, onError };
  useEffect(() => {
    const video = videoRef.current;
    if (!video) return;
    const stop = startCameraScan(
      video,
      (text) => latest.current.onText(text),
      (error) => latest.current.onError(error),
      deps,
    );
    return stop;
  }, [deps]);
  return (
    <video
      ref={videoRef}
      aria-label="相机预览"
      autoPlay
      muted
      playsInline
      className="h-full w-full object-cover"
    />
  );
}
