import { describe, expect, it, vi } from "vitest";
import { createBoard, listBoards, removeBoard } from "./superpowersKanbanBoards";

type Rpc = Parameters<typeof listBoards>[0];

describe("board lifecycle RPCs", () => {
  it("createBoard 先装插件、再走通用 plugin/query 建盘", async () => {
    const calls: string[] = [];
    const inner = vi.fn(async (method: string) => {
      calls.push(method);
      return {};
    });
    await createBoard(inner as unknown as Rpc, "/proj");
    // 顺序是契约：没有清单，宿主根本不知道要拉起这个项目的插件，
    // plugin/query 也就无处可拨。
    expect(calls).toEqual(["plugin/install", "plugin/query"]);
    expect(inner).toHaveBeenNthCalledWith(1, "plugin/install", {
      project: "/proj",
      plugin: "superpowers-kanban",
    });
    expect(inner).toHaveBeenNthCalledWith(2, "plugin/query", {
      plugin: "superpowers-kanban",
      project: "/proj",
      method: "lifecycle/create",
      params: { project: "/proj" },
    });
  });

  it("安装失败就不建盘：不能留下一个永远无法推进的看板", async () => {
    const inner = vi.fn(async () => {
      throw new Error("plugin_not_installed");
    });
    await expect(createBoard(inner as unknown as Rpc, "/proj")).rejects.toThrow(
      "plugin_not_installed",
    );
    expect(inner).toHaveBeenCalledTimes(1);
  });

  it("removeBoard 先卸载插件、再用 plugin/query 兜底", async () => {
    const calls: string[] = [];
    const inner = vi.fn(async (method: string) => {
      calls.push(method);
      return {};
    });
    await removeBoard(inner as unknown as Rpc, "/proj");
    // 顺序是契约：只发 lifecycle/remove 的话，daemon 还在跑、清单还在，
    // 插件下一拍就把状态目录重建出来——「移除」等于「重启」。
    expect(calls).toEqual(["plugin/uninstall", "plugin/query"]);
    expect(inner).toHaveBeenNthCalledWith(1, "plugin/uninstall", {
      project: "/proj",
      plugin: "superpowers-kanban",
    });
    expect(inner).toHaveBeenNthCalledWith(2, "plugin/query", {
      plugin: "superpowers-kanban",
      project: "/proj",
      method: "lifecycle/remove",
      params: { project: "/proj" },
    });
  });

  it("卸载后插件已消失：兜底那一下失败不影响移除成功", async () => {
    const inner = vi.fn(async (method: string) => {
      if (method === "plugin/query") throw new Error("plugin_unavailable");
      return {};
    });
    // 卸载成功即视为移除成功；兜底失败不能让用户以为没删掉、再点一次。
    await expect(removeBoard(inner as unknown as Rpc, "/proj")).resolves.toBeUndefined();
  });

  it("卸载本身失败时把错误抛给调用方（不能假装删掉了）", async () => {
    const inner = vi.fn(async (method: string) => {
      if (method === "plugin/uninstall") throw new Error("boom");
      return {};
    });
    await expect(removeBoard(inner as unknown as Rpc, "/proj")).rejects.toThrow("boom");
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
