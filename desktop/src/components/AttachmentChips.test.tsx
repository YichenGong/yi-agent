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

/**
 * 宿主注入的读取接缝。`call` 现在是**必填** prop：此前它可选并带一个 `NO_READ`
 * 兜底，任何漏注入 `call` 的嵌入都会静默渲染成「读取失败」的 chip（移除测试其实
 * 从未走到那条路——文档 chip 不取字节、`threadId=null` 也在 `call` 之前短路）。
 * 模块级常量保证引用稳定，与真实宿主 `App` 的 `imageCall` 同理。
 */
const imageCall = vi.fn(async () => ({}));

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
        call={imageCall}
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
        call={imageCall}
      />,
    );
    // The remove button names WHICH attachment: two chips must not be ambiguous.
    fireEvent.click(screen.getByRole("button", { name: "移除 notes.docx" }));
    expect(onRemove).toHaveBeenCalledWith("/tmp/notes.docx");
    expect(onRemove).toHaveBeenCalledTimes(1);
  });

  it("renders nothing when there are no attachments", () => {
    const { container } = render(
      <AttachmentChips attachments={[]} onRemove={() => {}} call={imageCall} />,
    );
    expect(container.firstChild).toBeNull();
  });

  // 生产形态：桌面端文件选择器只给路径，`App.tsx` 一律以 `size: 0` 入列。
  // 0 意为「未知」，chip 不能把它渲染成 "0 B"（假装知道大小）。
  it("omits the size when it is unknown (size: 0), keeping the label and remove button", () => {
    render(
      <AttachmentChips
        attachments={[{ path: "/tmp/picked.pdf", name: "picked.pdf", size: 0, kind: "document" }]}
        onRemove={() => {}}
        call={imageCall}
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
        call={imageCall}
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
        call={imageCall}
      />,
    );
    expect(screen.getByText("report.pdf").getAttribute("title")).toBe(
      "/tmp/a-very-long-dir-name/report.pdf",
    );
  });

  // 图片 chip 的缩略图：字节由 `image/read` 另取，chip 只负责把 hook 给出的
  // 状态画出来。锚点是 data-testid，App/Send 的联合测试也靠它。
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
        // 这一行就是待发列表：生产里 App 恒以 pending 渲染，故此处的默认值也是它。
        pending
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

    it("shows a neutral placeholder for a pending chip whose bytes cannot be read", () => {
      // 待发图片带的是 OS 文件对话框给的**绝对**路径（如 /tmp/截图.png），而
      // `image/read` 以会话 cwd 为根（安全边界），这类路径必然读不回来。这是
      // 预期内的事实，不是错误：必须画中性占位 + 「发送后可预览」，绝不画红色 ✗
      // ——那会让一张完全正常的待发图片看起来像坏了。
      hook.result = { url: null, error: "image/read failed" };
      renderChips([{ path: "/tmp/截图.png", name: "截图.png", size: 0, kind: "image" }]);

      expect(screen.queryByTestId("attachment-thumb-error")).toBeNull();
      const thumb = screen.getByTestId("attachment-thumb");
      expect(thumb.tagName).not.toBe("IMG");
      expect(thumb.getAttribute("title")).toBe("发送后可预览");
      // 中性占位也不吃掉移除入口。
      expect(screen.getByRole("button", { name: "移除 截图.png" })).toBeTruthy();
    });

    it("marks a read failure when the chip is not pending (its path should be readable)", () => {
      // 反面：路径**本该**可读（如服务端回显的 cwd 相对路径）却读失败，才是真错误，
      // 此时必须留下可见的失败标记，不能悄悄退化成中性占位。
      hook.result = { url: null, error: "boom" };
      renderChips(
        [{ path: ".yi-agent/attachments/pic.png", name: "pic.png", size: 0, kind: "image" }],
        { pending: false },
      );

      expect(thumbnailImages()).toHaveLength(0);
      expect(screen.getByTestId("attachment-thumb-error")).toBeTruthy();
      // 出错也不能吃掉移除入口：坏图更要能一键摘掉。
      expect(screen.getByRole("button", { name: "移除 pic.png" })).toBeTruthy();
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
