/** @vitest-environment jsdom */
import { describe, it, expect, afterEach, vi } from "vitest";
import { render, screen, fireEvent, cleanup } from "@testing-library/react";
import { ToolCallCard } from "./ToolCallCard";
import type { Item } from "../lib/protocol";

type ToolCallItem = Extract<Item, { type: "toolCall" }>;

// 图片字节由 `image/read` 另取；这里只验证卡片把 hook 给出的 URL 画成 <img>。
vi.mock("../lib/useImageData", () => ({
  useImageData: () => ({ url: "blob:tool-image", error: null }),
}));

// 稳定的读取接缝（组件只在有 `call` 时才画图片）。
const call = async () => ({ data: "", nextOffset: null, mediaType: "image/png", size: 0 });

afterEach(cleanup);

const bash = (input: unknown, over: Partial<ToolCallItem> = {}): ToolCallItem => ({
  type: "toolCall",
  id: "i1",
  call_id: "c1",
  name: "bash",
  input,
  status: "completed",
  ...over,
});

describe("ToolCallCard", () => {
  it("shows the command in the collapsed header", () => {
    render(<ToolCallCard item={bash({ command: "git status --short" })} />);
    expect(screen.getByText("git status --short")).toBeTruthy();
  });

  it("shows the path for a read", () => {
    render(<ToolCallCard item={bash({ path: "src/App.tsx" }, { name: "read" })} />);
    expect(screen.getByText("src/App.tsx")).toBeTruthy();
  });

  it("falls back to the bare tool name for an unknown tool", () => {
    render(<ToolCallCard item={bash(42, { name: "mystery" })} />);
    expect(screen.getByText("mystery")).toBeTruthy();
  });

  it("still renders the full input JSON when expanded", () => {
    const { container } = render(<ToolCallCard item={bash({ command: "ls -la" })} />);
    fireEvent.click(screen.getByRole("button"));
    expect(container.textContent).toContain('"command": "ls -la"');
  });

  it("labels the summary for hover and screen readers", () => {
    render(<ToolCallCard item={bash({ command: "ls -la" })} />);
    expect(screen.getByTitle("ls -la")).toBeTruthy();
  });

  it("shows a placeholder when the tool name is empty", () => {
    render(<ToolCallCard item={bash({ command: "ls -la" }, { name: "" })} />);
    expect(screen.getByText("(unknown tool)")).toBeTruthy();
  });

  it("renders image refs from the tool result", () => {
    const { getAllByTestId } = render(
      <ToolCallCard
        item={bash(
          { path: "shot.png" },
          {
            name: "screenshot",
            // 工具结果里的 size 恒为 0（未知），不得据此跳过渲染。
            images: [
              { path: ".yi-agent/attachments/t1/ab-shot.png", media_type: "image/png", size: 0 },
            ],
          },
        )}
        threadId="t1"
        call={call}
      />,
    );
    // 图片与结果同属展开区（在 Result 之前），展开后可见。
    fireEvent.click(screen.getByRole("button"));
    const imgs = getAllByTestId("tool-image");
    expect(imgs).toHaveLength(1);
    expect(imgs[0].getAttribute("src")).toBe("blob:tool-image");
  });

  it("renders no image when the tool result has no refs", () => {
    const { queryAllByTestId } = render(<ToolCallCard item={bash({ command: "ls" })} />);
    expect(queryAllByTestId("tool-image")).toHaveLength(0);
  });
});
