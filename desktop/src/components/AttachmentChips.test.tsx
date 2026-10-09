/** @vitest-environment jsdom */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { render, screen, fireEvent, cleanup } from "@testing-library/react";

// 图片字节由 `image/read` 另取（见 `useImageData`）：本文件只验证 chip 把 hook
// 给出的状态画成什么，真实的分片/缓存行为在 `lib/useImageData.test.ts` 里测。
// 记录调用参数是为了断言「只对图片 chip 调、且 threadId/call 原样透传」。
const hook = vi.hoisted(() => ({
  calls: [] as Array<{ path: string; threadId: string | null; call: unknown }>,
  result: { url: null as string | null, error: null as string | null },
}));

vi.mock("../lib/useImageData", () => ({
  useImageData: (path: string, opts: { threadId: string | null; call: unknown }) => {
    hook.calls.push({ path, ...opts });
    return hook.result;
  },
}));

import { AttachmentChips } from "./AttachmentChips";

/** 缩略图里真正的 `<img>` 节点（占位块用的是同一个 testid，但不是 img）。 */
function thumbnailImages(): HTMLImageElement[] {
  return screen
    .queryAllByTestId("attachment-thumb")
    .filter((el): el is HTMLImageElement => el.tagName === "IMG");
}

beforeEach(() => {
  hook.calls = [];
  hook.result = { url: null, error: null };
});

afterEach(cleanup);

describe("AttachmentChips", () => {
  it("renders one chip per attachment with its name and size", () => {
    render(
      <AttachmentChips
        attachments={[
          { path: "/tmp/report.pdf", name: "report.pdf", size: 2048, kind: "document" },
          { path: "/tmp/notes.docx", name: "notes.docx", size: 512, kind: "document" },
        ]}
        onRemove={() => {}}
      />,
    );
    // The parent needs a stable handle on the row itself (layout/spacing tests in
    // App), so the test id is part of the contract.
    expect(screen.getByTestId("attachment-chips")).toBeTruthy();
    expect(screen.getByText("report.pdf")).toBeTruthy();
    expect(screen.getByText("notes.docx")).toBeTruthy();
    // Sizes are human-readable, not raw bytes.
    expect(screen.getByText("2 KB")).toBeTruthy();
    expect(screen.getByText("512 B")).toBeTruthy();
  });

  it("reports the removed path", () => {
    const onRemove = vi.fn();
    render(
      <AttachmentChips
        attachments={[
          { path: "/tmp/report.pdf", name: "report.pdf", size: 2048, kind: "document" },
          { path: "/tmp/notes.docx", name: "notes.docx", size: 512, kind: "document" },
        ]}
        onRemove={onRemove}
      />,
    );
    // The remove button names WHICH attachment: two chips must not be ambiguous.
    fireEvent.click(screen.getByRole("button", { name: "移除 notes.docx" }));
    expect(onRemove).toHaveBeenCalledWith("/tmp/notes.docx");
    expect(onRemove).toHaveBeenCalledTimes(1);
  });

  it("renders nothing when there are no attachments", () => {
    const { container } = render(<AttachmentChips attachments={[]} onRemove={() => {}} />);
    expect(container.firstChild).toBeNull();
  });

  // 生产形态：桌面端文件选择器只给路径，`App.tsx` 一律以 `size: 0` 入列。
  // 0 意为「未知」，chip 不能把它渲染成 "0 B"（假装知道大小）。
  it("omits the size when it is unknown (size: 0), keeping the label and remove button", () => {
    render(
      <AttachmentChips
        attachments={[{ path: "/tmp/picked.pdf", name: "picked.pdf", size: 0, kind: "document" }]}
        onRemove={() => {}}
      />,
    );
    expect(screen.getByText("picked.pdf")).toBeTruthy();
    expect(screen.getByRole("button", { name: "移除 picked.pdf" })).toBeTruthy();
    expect(screen.queryByText("0 B")).toBeNull();
  });

  it("falls back to the path's last segment when the name is empty", () => {
    render(
      <AttachmentChips
        attachments={[{ path: "/tmp/dir/fallback.pdf", name: "", size: 1024 * 1024 * 1.5, kind: "document" }]}
        onRemove={() => {}}
      />,
    );
    expect(screen.getByText("fallback.pdf")).toBeTruthy();
    expect(screen.getByRole("button", { name: "移除 fallback.pdf" })).toBeTruthy();
    expect(screen.getByText("1.5 MB")).toBeTruthy();
  });

  it("exposes the full path as a tooltip while truncating long names", () => {
    render(
      <AttachmentChips
        attachments={[{ path: "/tmp/a-very-long-dir-name/report.pdf", name: "report.pdf", size: 10, kind: "document" }]}
        onRemove={() => {}}
      />,
    );
    expect(screen.getByText("report.pdf").getAttribute("title")).toBe(
      "/tmp/a-very-long-dir-name/report.pdf",
    );
  });

  // 图片 chip 的缩略图：字节由 `image/read` 另取，chip 只负责把 hook 给出的
  // 状态画出来。锚点是 data-testid，App/Send 的联合测试也靠它。
  const imageCall = vi.fn(async () => ({}));

  function renderChips(
    attachments: Parameters<typeof AttachmentChips>[0]["attachments"],
    overrides: Partial<Parameters<typeof AttachmentChips>[0]> = {},
  ) {
    const onRemove = vi.fn();
    render(
      <AttachmentChips
        attachments={attachments}
        onRemove={onRemove}
        threadId="t1"
        call={imageCall}
        {...overrides}
      />,
    );
    return { onRemove };
  }

  describe("thumbnails", () => {
    it("renders the thumbnail for an image chip and keeps it removable", () => {
      hook.result = { url: "blob:thumb", error: null };
      const { onRemove } = renderChips([
        { path: "/tmp/截图.png", name: "截图.png", size: 0, kind: "image" },
      ]);

      const thumb = screen.getByTestId("attachment-thumb");
      expect(thumb.tagName).toBe("IMG");
      expect(thumb.getAttribute("src")).toBe("blob:thumb");
      // 有缩略图也要有名字与叉：thumb 是补充信息，不替代 chip 本身。
      expect(screen.getByText("截图.png")).toBeTruthy();
      fireEvent.click(screen.getByRole("button", { name: "移除 截图.png" }));
      expect(onRemove).toHaveBeenCalledWith("/tmp/截图.png");
      // host 的 threadId / 稳定 call 原样透传给 hook（否则要么读不到、要么每次
      // 渲染重拉）。
      expect(hook.calls).toEqual([
        { path: "/tmp/截图.png", threadId: "t1", call: imageCall },
      ]);
    });

    it("renders a placeholder before the bytes are ready", () => {
      // 未就绪（url 为 null 且无错）时给一个中性占位块，但绝不画空 <img>：
      // 空 src 的 img 会在窄行里闪一下破图。
      renderChips([{ path: "/tmp/截图.png", name: "截图.png", size: 0, kind: "image" }]);

      const thumb = screen.getByTestId("attachment-thumb");
      expect(thumb.tagName).not.toBe("IMG");
      expect(thumbnailImages()).toHaveLength(0);
    });

    it("marks a read failure instead of showing a broken image", () => {
      hook.result = { url: null, error: "boom" };
      renderChips([{ path: "/tmp/截图.png", name: "截图.png", size: 0, kind: "image" }]);

      expect(thumbnailImages()).toHaveLength(0);
      expect(screen.getByTestId("attachment-thumb-error")).toBeTruthy();
      // 出错也不能吃掉移除入口：坏图更要能一键摘掉。
      expect(screen.getByRole("button", { name: "移除 截图.png" })).toBeTruthy();
    });

    it("renders no thumbnail for a document chip and never asks for its bytes", () => {
      hook.result = { url: "blob:thumb", error: null };
      renderChips([
        { path: "/tmp/report.pdf", name: "report.pdf", size: 10, kind: "document" },
      ]);

      expect(screen.queryByTestId("attachment-thumb")).toBeNull();
      // 文档走 `read_document` 清单，不在这里取字节：hook 不该被调用。
      expect(hook.calls).toHaveLength(0);
    });

    it("thumbnails only the image chips of a mixed row", () => {
      hook.result = { url: "blob:thumb", error: null };
      renderChips([
        { path: "/tmp/截图.png", name: "截图.png", size: 0, kind: "image" },
        { path: "/tmp/report.pdf", name: "report.pdf", size: 10, kind: "document" },
      ]);

      const images = thumbnailImages();
      expect(images).toHaveLength(1);
      expect(images[0].getAttribute("src")).toBe("blob:thumb");
      expect(hook.calls.map((c) => c.path)).toEqual(["/tmp/截图.png"]);
    });
  });
});
