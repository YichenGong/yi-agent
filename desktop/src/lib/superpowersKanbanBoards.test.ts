import { describe, expect, it, vi } from "vitest";
import { createBoard, listBoards, removeBoard } from "./superpowersKanbanBoards";

type Rpc = Parameters<typeof listBoards>[0];

describe("board lifecycle RPCs", () => {
  it("createBoard 带上项目路径", async () => {
    const inner = vi.fn(async () => ({}));
    await createBoard(inner as unknown as Rpc, "/proj");
    expect(inner).toHaveBeenCalledWith("board/create", { project: "/proj" });
  });

  it("removeBoard 带上项目路径", async () => {
    const inner = vi.fn(async () => ({}));
    await removeBoard(inner as unknown as Rpc, "/proj");
    expect(inner).toHaveBeenCalledWith("board/remove", { project: "/proj" });
  });

  it("createBoard 把失败原样抛给调用方（不吞）", async () => {
    const inner = vi.fn(async () => {
      throw new Error("board_not_created");
    });
    await expect(createBoard(inner as unknown as Rpc, "/proj")).rejects.toThrow(
      "board_not_created",
    );
  });
});

describe("listBoards", () => {
  it("读出 app-server 的真实形状（items 带 project/created_at/status）", async () => {
    const inner = vi.fn(async () => ({
      boards: [
        { project: "/a", created_at: 1, status: { registered: true, daemon_running: true } },
        { project: "/b", created_at: 2, status: { registered: true, daemon_running: false } },
      ],
    }));
    expect(await listBoards(inner as unknown as Rpc)).toEqual(["/a", "/b"]);
    expect(inner).toHaveBeenCalledWith("board/list", {});
  });

  it("也容忍裸字符串数组", async () => {
    const inner = vi.fn(async () => ({ boards: ["/a", "/b"] }));
    expect(await listBoards(inner as unknown as Rpc)).toEqual(["/a", "/b"]);
  });

  it("也容忍顶层数组与 items 字段", async () => {
    const bare = vi.fn(async () => ["/a"]);
    expect(await listBoards(bare as unknown as Rpc)).toEqual(["/a"]);

    const items = vi.fn(async () => ({ items: [{ project: "/a" }] }));
    expect(await listBoards(items as unknown as Rpc)).toEqual(["/a"]);
  });

  it("形状不认识时不抛，只当没有看板", async () => {
    // 侧栏靠这个列表决定画不画看板条目：读不出来就是没有，绝不能整页崩。
    for (const shape of [null, undefined, {}, { boards: null }, { boards: [42, {}, null] }]) {
      const inner = vi.fn(async () => shape);
      expect(await listBoards(inner as unknown as Rpc)).toEqual([]);
    }
  });

  it("跳过没有 project 的条目，保留有 project 的", async () => {
    const inner = vi.fn(async () => ({ boards: [{ project: "/a" }, { created_at: 3 }, "/b"] }));
    expect(await listBoards(inner as unknown as Rpc)).toEqual(["/a", "/b"]);
  });
});
