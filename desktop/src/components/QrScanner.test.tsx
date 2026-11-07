/** @vitest-environment jsdom */
import { afterEach, expect, it, vi } from "vitest";
import { cleanup, render } from "@testing-library/react";
import { QrScanner } from "./QrScanner";
import type { ScanDeps } from "../lib/qrScanner";
afterEach(() => { cleanup(); vi.useRealTimers(); vi.restoreAllMocks(); });
function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((res, rej) => { resolve = res; reject = rej; });
  return { promise, resolve, reject };
}
it("A05 callback refresh does not reopen camera", async () => {
  vi.useFakeTimers();
  vi.spyOn(HTMLMediaElement.prototype, "play").mockResolvedValue(undefined);
  const trackStop = vi.fn();
  const stream = { getTracks: () => [{ stop: trackStop }] } as unknown as MediaStream;
  const getUserMedia = vi.fn(async () => stream);
  const decode = vi.fn<() => string | null>(() => null);
  const deps: ScanDeps = {
    getUserMedia, decode,
    grabFrame: () => ({ data: new Uint8ClampedArray(4), width: 1, height: 1 }) as ImageData,
  };
  const first = vi.fn(() => false);
  const latest = vi.fn(() => true);
  const view = render(<QrScanner deps={deps} onText={first} onError={vi.fn()} />);
  await vi.advanceTimersByTimeAsync(0);
  view.rerender(<QrScanner deps={deps} onText={latest} onError={vi.fn()} />);
  await vi.advanceTimersByTimeAsync(0);
  expect(getUserMedia).toHaveBeenCalledTimes(1);
  expect(trackStop).not.toHaveBeenCalled();
  decode.mockReturnValue("hit");
  await vi.advanceTimersByTimeAsync(200);
  expect(first).not.toHaveBeenCalled();
  expect(latest).toHaveBeenCalledWith("hit");
  view.unmount();
  expect(trackStop).toHaveBeenCalledTimes(1);
});
it.each(["stream", "play"] as const)("A11 old %s cannot overwrite the new scan", async phase => {
  vi.useFakeTimers();
  const oldStream = deferred<MediaStream>();
  const oldPlay = deferred<void>();
  const oldStop = vi.fn();
  const newStop = vi.fn();
  const oldMedia = { getTracks: () => [{ stop: oldStop }] } as unknown as MediaStream;
  const newMedia = { getTracks: () => [{ stop: newStop }] } as unknown as MediaStream;
  const play = vi.spyOn(HTMLMediaElement.prototype, "play");
  if (phase === "play") play.mockReturnValueOnce(oldPlay.promise);
  play.mockResolvedValue(undefined);
  const oldDeps: ScanDeps = { getUserMedia: () => phase === "stream" ? oldStream.promise : Promise.resolve(oldMedia), grabFrame: () => null };
  const newDeps: ScanDeps = { getUserMedia: async () => newMedia, grabFrame: () => null };
  const oldError = vi.fn();
  const newError = vi.fn();
  const view = render(<QrScanner deps={oldDeps} onText={() => false} onError={oldError} />);
  await vi.advanceTimersByTimeAsync(0);
  view.rerender(<QrScanner deps={newDeps} onText={() => false} onError={newError} />);
  await vi.advanceTimersByTimeAsync(0);
  const video = view.container.querySelector("video")!;
  expect(video.srcObject).toBe(newMedia);
  if (phase === "stream") oldStream.resolve(oldMedia);
  else oldPlay.reject(new Error("old play failed"));
  await vi.advanceTimersByTimeAsync(0);
  expect(video.srcObject).toBe(newMedia);
  expect(oldError).not.toHaveBeenCalled();
  expect(newError).not.toHaveBeenCalled();
  expect(oldStop).toHaveBeenCalledTimes(1);
  expect(newStop).not.toHaveBeenCalled();
  view.unmount();
  expect(newStop).toHaveBeenCalledTimes(1);
});
