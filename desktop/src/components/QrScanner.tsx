import { useEffect, useRef } from "react";
import { startCameraScan, type ScanDeps } from "../lib/qrScanner";

/** 全屏相机预览 + 扫描循环的薄 UI。逻辑全在 `lib/qrScanner`。 */
export interface QrScannerProps {
  onText: (text: string) => void;
  onError: (e: unknown) => void;
  /** 注入接缝（测试用）；生产省略。 */
  deps?: ScanDeps;
}

export function QrScanner({ onText, onError, deps }: QrScannerProps) {
  const videoRef = useRef<HTMLVideoElement | null>(null);
  useEffect(() => {
    const video = videoRef.current;
    if (!video) return;
    const stop = startCameraScan(video, onText, onError, deps);
    return stop;
  }, [onText, onError, deps]);
  return (
    <video
      ref={videoRef}
      aria-label="相机预览"
      playsInline
      className="h-full w-full object-cover"
    />
  );
}
