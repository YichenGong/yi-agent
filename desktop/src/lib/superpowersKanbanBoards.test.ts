import { describe, expect, it, vi } from "vitest";
import { createBoard, listBoards, removeBoard } from "./superpowersKanbanBoards";

type Rpc = Parameters<typeof listBoards>[0];

describe("board lifecycle RPCs", () => {
  it("createBoard 走通用 plugin/query（路由 project 与 params.project 都带）", async () => {
    const inner = vi.fn(async () => ({}));
    await createBoard(inner as unknown as Rpc, "/proj");
    expect(inner).toHaveBeenCalledWith("plugin/query", {
      plugin: "superpowers-kanban",
      project: "/proj",
      method: "lifecycle/create",
      params: { project: "/proj" },
    });
  });

  it("removeBoard 走通用 plugin/query", async () => {
    const inner = vi.fn(async () => ({}));
    await removeBoard(inner as unknown as Rpc, "/proj");
    expect(inner).toHaveBeenCalledWith("plugin/query", {
      plugin: "superpowers-kanban",
      project: "/proj",
      method: "lifecycle/remove",
      params: { project: "/proj" },
    });
  });

  it("createBoard 把失败原样抛给调用方（不吞）", async () => {
    const inner = vi.fn(async () => {
      throw new Error("daemon_unavailable");
    });
    await expect(createBoard(inner as unknown as Rpc, "/proj")).rejects.toThrow(
      "daemon_unavailable",
    );
  });
});

describe("listBoards", () => {
  it("从通用 resident/list 读已登记项目", async () => {
    const inner = vi.fn(async () => ({ projects: [{ project: "/a" }, { project: "/b" }] }));
    expect(await listBoards(inner as unknown as Rpc)).toEqual(["/a", "/b"]);
    expect(inner).toHaveBeenCalledWith("resident/list", {});
  });

  it("也容忍裸字符串数组", async () => {
    const inner = vi.fn(async () => ({ projects: ["/a", "/b"] }));
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
    for (const shape of [null, undefined, {}, { projects: null }, { projects: [42, {}, null] }]) {
      const inner = vi.fn(async () => shape);
      expect(await listBoards(inner as unknown as Rpc)).toEqual([]);
    }
  });

  it("跳过没有 project 的条目，保留有 project 的", async () => {
    const inner = vi.fn(async () => ({
      projects: [{ project: "/a" }, { required_by: ["superpowers-kanban"] }, "/b"],
    }));
    expect(await listBoards(inner as unknown as Rpc)).toEqual(["/a", "/b"]);
  });
});
