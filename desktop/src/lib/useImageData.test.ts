/** @vitest-environment jsdom */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { renderHook, waitFor, cleanup } from "@testing-library/react";

const PATH = ".yi-agent/attachments/t1/ab-shot.png";

// The LRU lives in module scope, so every case must start from a *fresh module
// instance*: `vi.resetModules()` + a dynamic import gives us one. Without this,
// the cache-hit case could not distinguish itself from a cache primed by the
// previous case.
let mod: typeof import("./useImageData");
let blobs: Blob[];
let createdUrls: string[];
let revokedUrls: string[];
let urlSeq: number;

beforeEach(async () => {
  vi.resetModules();
  mod = await import("./useImageData");
  blobs = [];
  createdUrls = [];
  revokedUrls = [];
  urlSeq = 0;
  // jsdom ships neither `URL.createObjectURL` nor `revokeObjectURL`; a
  // observable stand-in is what lets the tests assert "one Blob per fetch" and
  // "eviction revokes".
  URL.createObjectURL = vi.fn((blob: Blob) => {
    blobs.push(blob);
    const url = `blob:image-${++urlSeq}`;
    createdUrls.push(url);
    return url;
  });
  URL.revokeObjectURL = vi.fn((url: string) => {
    revokedUrls.push(url);
  });
});

afterEach(cleanup);

/** Read a Blob back to raw bytes (jsdom has no `Blob.arrayBuffer`). */
function blobBytes(blob: Blob): Promise<number[]> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onerror = () => reject(reader.error);
    reader.onload = () =>
      resolve(Array.from(new Uint8Array(reader.result as ArrayBuffer)));
    reader.readAsArrayBuffer(blob);
  });
}

const b64 = (bytes: number[]) => btoa(String.fromCharCode(...bytes));

/**
 * A fake `image/read` server that serves `fileBytes` in `chunkSize`-byte
 * chunks, exactly like the real RPC: `nextOffset` is `null` on the last chunk.
 */
function serving(fileBytes: number[], chunkSize = 3) {
  const requests: Array<{ method: string; params: Record<string, unknown> }> = [];
  const call = vi.fn(async (method: string, params: unknown) => {
    expect(method).toBe("image/read");
    const p = params as { path?: string; offset?: number };
    requests.push({ method, params: { ...p } });
    const offset = p.offset ?? 0;
    const slice = fileBytes.slice(offset, offset + chunkSize);
    const next = offset + chunkSize >= fileBytes.length ? null : offset + chunkSize;
    return {
      data: b64(slice),
      nextOffset: next,
      mediaType: "image/png",
      size: fileBytes.length,
    };
  });
  return { call, requests };
}

describe("useImageData", () => {
  it("loops image/read by nextOffset and concatenates the raw bytes", async () => {
    const file = [1, 2, 3, 4, 5, 6, 7];
    const { call, requests } = serving(file);
    const { result } = renderHook(() =>
      mod.useImageData(PATH, { threadId: "t1", call }),
    );

    await waitFor(() => expect(result.current.url).toBeTruthy());

    // One request per chunk, each carrying the previous response's nextOffset.
    expect(requests.map((r) => r.params.offset)).toEqual([0, 3, 6]);
    expect(requests[0].params).toMatchObject({ threadId: "t1", path: PATH, offset: 0 });
    // The Blob must carry the file's bytes verbatim — `size` from the response
    // is the stored length, not a limit on what we decode.
    expect(await blobBytes(blobs[0])).toEqual(file);
    expect(blobs[0].type).toBe("image/png");
    expect(result.current.error).toBeNull();
  });

  it("serves a second consumer of the same path from the cache with no new request", async () => {
    const { call, requests } = serving([10, 20, 30, 40]);
    const first = renderHook(() => mod.useImageData(PATH, { threadId: "t1", call }));
    await waitFor(() => expect(first.result.current.url).toBeTruthy());

    const seen = requests.length;
    const second = renderHook(() => mod.useImageData(PATH, { threadId: "t1", call }));
    await waitFor(() =>
      expect(second.result.current.url).toBe(first.result.current.url),
    );

    // Same path → same URL, zero extra RPC round-trips, one Blob total.
    expect(requests.length).toBe(seen);
    expect(createdUrls).toHaveLength(1);
  });

  it("surfaces a read failure as an error", async () => {
    const call = vi.fn(async () => {
      throw { code: -32000, message: "no such image" };
    });
    const { result } = renderHook(() =>
      mod.useImageData(PATH, { threadId: "t1", call }),
    );

    await waitFor(() => expect(result.current.error).toBe("no such image"));
    expect(result.current.url).toBeNull();
  });

  it("does not read without a thread or a path", async () => {
    const call = vi.fn(async () => ({ data: "", nextOffset: null, mediaType: "", size: 0 }));
    const noThread = renderHook(() =>
      mod.useImageData(PATH, { threadId: null, call }),
    );
    const noPath = renderHook(() => mod.useImageData("", { threadId: "t1", call }));
    await Promise.resolve();

    expect(call).not.toHaveBeenCalled();
    expect(noThread.result.current).toEqual({ url: null, error: null });
    expect(noPath.result.current).toEqual({ url: null, error: null });
  });

  it("evicts the least recently used entry past the cap and revokes its URL", async () => {
    const { call } = serving([1], 1);
    const paths = Array.from(
      { length: mod.IMAGE_CACHE_LIMIT + 1 },
      (_, i) => `.yi-agent/attachments/t1/x-${i}.png`,
    );
    for (const p of paths) {
      const h = renderHook(() => mod.useImageData(p, { threadId: "t1", call }));
      await waitFor(() => expect(h.result.current.url).toBeTruthy(), { interval: 1 });
    }

    // The first path fell out of the LRU window: its object URL is released so
    // a long session cannot leak every image it ever displayed.
    expect(revokedUrls).toEqual([createdUrls[0]]);
  });
});
