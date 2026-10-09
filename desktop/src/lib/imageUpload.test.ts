/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest";
import { UPLOAD_CHUNK_BYTES, uploadImage } from "./imageUpload";

/**
 * 远端（iOS）分片上传客户端。
 *
 * 全部用 mock 的 `call` 接缝（与 `imageCall`/`modelCall` 同形的普通函数），不发真
 * 请求：这里要验证的是**协议序列**（begin 一次 → chunk 按 index 递增 → commit 一
 * 次）与切块边界，而不是 RPC 本身。
 */

const COMMITTED_PATH = ".yi-agent/attachments/t1/ab-截图.png";

/** 第 i 字节 = i % 251 的确定内容，便于逐字节比对拼装结果。 */
function byteSequence(size: number): number[] {
  return Array.from({ length: size }, (_, i) => i % 251);
}

function makeFile(size: number, name = "截图.png", type = "image/png"): File {
  return new File([new Uint8Array(byteSequence(size))], name, { type });
}

/** 逐字节比对（大数组上比 `toEqual` 快得多，且失败信息定位到具体字节）。 */
function expectSameBytes(actual: number[], expected: number[]): void {
  expect(actual.length).toBe(expected.length);
  for (let i = 0; i < expected.length; i += 1) {
    if (actual[i] !== expected[i]) {
      throw new Error(`byte ${i}: got ${actual[i]}, want ${expected[i]}`);
    }
  }
}

/** 一个会说 `image/upload/*` 的假服务端，记录往返与收到的字节。 */
function serving(opts: { failBegin?: boolean; failChunkAt?: number; failAbort?: boolean } = {}) {
  const calls: Array<{ method: string; params: Record<string, unknown> }> = [];
  const received: number[] = [];
  const call = async (method: string, params: unknown): Promise<unknown> => {
    const p = (params ?? {}) as Record<string, unknown>;
    calls.push({ method, params: p });
    switch (method) {
      case "image/upload/begin":
        if (opts.failBegin) throw { code: -32602, message: "begin refused" };
        return { uploadId: "u1", chunkSize: UPLOAD_CHUNK_BYTES };
      case "image/upload/chunk": {
        if (opts.failChunkAt !== undefined && p.index === opts.failChunkAt) {
          throw { code: -32602, message: "chunk refused" };
        }
        const bin = atob(p.data as string);
        for (let i = 0; i < bin.length; i += 1) received.push(bin.charCodeAt(i));
        return {};
      }
      case "image/upload/commit":
        return { path: COMMITTED_PATH, mediaType: "image/png", size: received.length };
      case "image/upload/abort":
        if (opts.failAbort) throw { code: -32602, message: "abort refused" };
        return {};
      default:
        throw new Error(`unexpected method: ${method}`);
    }
  };
  return {
    call,
    calls,
    received,
    methods: () => calls.map((c) => c.method),
    only: (method: string) => calls.filter((c) => c.method === method),
  };
}

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("uploadImage", () => {
  it("chunks a File by UPLOAD_CHUNK_BYTES, begins once, commits once, and returns the committed path", async () => {
    const size = UPLOAD_CHUNK_BYTES * 2 + 5;
    const file = makeFile(size);
    const s = serving();

    const out = await uploadImage(s.call, "t1", file);

    // begin 恰好一次，且报的是**最终**字节数与文件名/MIME。
    const begins = s.only("image/upload/begin");
    expect(begins).toHaveLength(1);
    expect(begins[0].params).toEqual({
      threadId: "t1",
      name: "截图.png",
      mime: "image/png",
      size,
    });

    // 三块：512 KiB / 512 KiB / 5 B，index 从 0 递增。
    const chunks = s.only("image/upload/chunk");
    expect(chunks).toHaveLength(3);
    expect(chunks.map((c) => c.params.index)).toEqual([0, 1, 2]);
    expect(chunks.map((c) => atob(c.params.data as string).length)).toEqual([
      UPLOAD_CHUNK_BYTES,
      UPLOAD_CHUNK_BYTES,
      5,
    ]);
    // 拼回去逐字节等于原文件：切片边界与顺序都对。
    expectSameBytes(s.received, byteSequence(size));

    // 顺序是契约：begin → 全部 chunk → commit。
    expect(s.methods()).toEqual([
      "image/upload/begin",
      "image/upload/chunk",
      "image/upload/chunk",
      "image/upload/chunk",
      "image/upload/commit",
    ]);
    expect(s.only("image/upload/commit")[0].params).toEqual({ uploadId: "u1" });
    // commit 返回的 path 就是调用方要的东西（chip 缩略图据此经 `image/read` 读回）。
    expect(out).toEqual({ uploadId: "u1", path: COMMITTED_PATH });
  });

  it("uploads a single sub-chunk file as exactly one chunk", async () => {
    const s = serving();
    await uploadImage(s.call, "t1", makeFile(7));

    expect(s.only("image/upload/begin")[0].params.size).toBe(7);
    expect(s.only("image/upload/chunk")).toHaveLength(1);
    expect(s.only("image/upload/chunk")[0].params.index).toBe(0);
    expectSameBytes(s.received, byteSequence(7));
  });

  it("aborts and rethrows when a chunk fails mid-upload", async () => {
    const s = serving({ failChunkAt: 1 });

    await expect(uploadImage(s.call, "t1", makeFile(UPLOAD_CHUNK_BYTES * 2))).rejects.toMatchObject({
      message: "chunk refused",
    });

    // 失败后 best-effort 收掉 staging（否则那半个文件占着名额活到 TTL）。
    expect(s.only("image/upload/abort")).toHaveLength(1);
    expect(s.only("image/upload/abort")[0].params).toEqual({ uploadId: "u1" });
    // 没有 commit：拼不全的文件绝不能当成功送出去。
    expect(s.only("image/upload/commit")).toHaveLength(0);
  });

  it("does not abort when begin itself is refused (nothing to abort yet)", async () => {
    const s = serving({ failBegin: true });

    await expect(uploadImage(s.call, "t1", makeFile(4))).rejects.toMatchObject({
      message: "begin refused",
    });

    expect(s.methods()).toEqual(["image/upload/begin"]);
  });

  it("keeps the original failure when abort itself fails", async () => {
    const s = serving({ failChunkAt: 0, failAbort: true });

    // abort 是尽力而为：它自己失败不能把真正的错误盖掉。
    await expect(uploadImage(s.call, "t1", makeFile(4))).rejects.toMatchObject({
      message: "chunk refused",
    });
    expect(s.only("image/upload/abort")).toHaveLength(1);
  });
});

describe("uploadImage / HEIC", () => {
  const JPEG_BYTES = [0xff, 0xd8, 0xff, 0xe0, 0x11, 0x22, 0x33, 0x44];
  /** HEIC 原始字节：与 JPEG 结果**不同**，用它能证明送上去的不是原文件。 */
  const HEIC_BYTES = Array.from({ length: 16 }, (_, i) => 0xa0 + i);

  /** 装好 createImageBitmap + canvas 的转码环境，返回被调用的桩。 */
  function stubTranscoder(blob: Blob | null = new Blob([new Uint8Array(JPEG_BYTES)], { type: "image/jpeg" })) {
    const bitmap = { width: 4, height: 3, close: vi.fn() };
    const createImageBitmap = vi.fn(async () => bitmap as unknown as ImageBitmap);
    vi.stubGlobal("createImageBitmap", createImageBitmap);
    const drawImage = vi.fn();
    vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockReturnValue({
      drawImage,
    } as unknown as CanvasRenderingContext2D);
    // jsdom 的 toBlob 没有实现（要 node-canvas），这里给一个立刻回调的桩。
    const toBlob = vi.spyOn(HTMLCanvasElement.prototype, "toBlob").mockImplementation((cb: BlobCallback) => {
      cb(blob);
    });
    return { createImageBitmap, drawImage, toBlob, bitmap };
  }

  function heicFile(name = "照片.HEIC", type = "image/heic"): File {
    return new File([new Uint8Array(HEIC_BYTES)], name, { type });
  }

  it("transcodes a HEIC file to JPEG before any upload request", async () => {
    const t = stubTranscoder();
    const s = serving();

    await uploadImage(s.call, "t1", heicFile());

    // 解码的是用户选中的那个 File，画布尺寸取自解码结果。
    expect(t.createImageBitmap).toHaveBeenCalledTimes(1);
    expect((t.createImageBitmap.mock.calls[0] as unknown[])[0]).toBeInstanceOf(Blob);
    expect(t.drawImage).toHaveBeenCalledTimes(1);
    expect(t.toBlob).toHaveBeenCalledTimes(1);
    // 位图尽早释放，别把解码后的像素钉在内存里。
    expect(t.bitmap.close).toHaveBeenCalledTimes(1);

    // 送上去的是 JPEG：名字换成 .jpg、MIME 换成 image/jpeg、大小是转码后的长度。
    const begin = s.only("image/upload/begin")[0].params;
    expect(begin).toEqual({
      threadId: "t1",
      name: "照片.jpg",
      mime: "image/jpeg",
      size: JPEG_BYTES.length,
    });
    // 字节就是 JPEG 的字节，绝不是 HEIC 原样。
    expectSameBytes(s.received, JPEG_BYTES);
  });

  it("treats a .heif name as HEIC even when the browser reports no type", async () => {
    stubTranscoder();
    const s = serving();

    // iOS 有时给空 type：此时只能看扩展名。
    await uploadImage(s.call, "t1", heicFile("IMG_0001.heif", ""));

    expect(s.only("image/upload/begin")[0].params.name).toBe("IMG_0001.jpg");
    expectSameBytes(s.received, JPEG_BYTES);
  });

  it("leaves a JPEG/PNG file untouched (no transcode)", async () => {
    const t = stubTranscoder();
    const s = serving();

    await uploadImage(s.call, "t1", makeFile(3, "a.png", "image/png"));

    expect(t.createImageBitmap).not.toHaveBeenCalled();
    expect(s.only("image/upload/begin")[0].params).toEqual({
      threadId: "t1",
      name: "a.png",
      mime: "image/png",
      size: 3,
    });
    expectSameBytes(s.received, byteSequence(3));
  });

  it("refuses a HEIC file when the platform cannot decode it (never uploads it raw)", async () => {
    vi.stubGlobal("createImageBitmap", undefined);
    const s = serving();

    await expect(uploadImage(s.call, "t1", heicFile())).rejects.toThrow(/HEIC/);
    // 关键：一个请求都没发出去——原样上传只会在服务端被拒，不如就地明说。
    expect(s.calls).toHaveLength(0);
  });

  it("refuses a HEIC file when decoding throws", async () => {
    vi.stubGlobal(
      "createImageBitmap",
      vi.fn(async () => {
        throw new Error("unsupported image format");
      }),
    );
    const s = serving();

    await expect(uploadImage(s.call, "t1", heicFile())).rejects.toThrow(/HEIC/);
    expect(s.calls).toHaveLength(0);
  });

  it("refuses a HEIC file when the canvas cannot encode a JPEG", async () => {
    stubTranscoder(null); // toBlob 回 null：编码失败
    const s = serving();

    await expect(uploadImage(s.call, "t1", heicFile())).rejects.toThrow(/HEIC/);
    expect(s.calls).toHaveLength(0);
  });
});
