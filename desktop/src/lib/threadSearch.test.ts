import { describe, expect, it } from "vitest";
import { filterThreadTree } from "./threadSearch";
import { resolveTagNames } from "./threadTags";
import type { ThreadSummary, WorkspaceGroup } from "./protocol";

function thread(id: string, title: string | null, cwd: string): ThreadSummary {
  return { thread_id: id, cwd, model: "m", created_at: 0, updated_at: 0, title };
}

const groups: WorkspaceGroup[] = [
  { workspace: "/work/projA", exists: true, threads: [thread("1", "fix login bug", "/work/projA")] },
  { workspace: "/work/projB", exists: true, threads: [thread("2", "write docs", "/work/projB")] },
];

/** 过滤后剩下的所有会话 id，按分组顺序摊平。 */
const idsOf = (r: ReturnType<typeof filterThreadTree>) =>
  r.groups.flatMap((g) => g.threads.map((t) => t.thread_id));

describe("filterThreadTree", () => {
  it("空查询原样返回，且 empty 为 false", () => {
    const r = filterThreadTree(groups, [], "");
    expect(r.groups).toBe(groups);
    expect(r.empty).toBe(false);
  });

  it("只有空白的查询同样原样返回", () => {
    const r = filterThreadTree(groups, [], "   \t ");
    expect(r.groups).toBe(groups);
    expect(r.empty).toBe(false);
  });

  it("大小写不敏感地匹配标题", () => {
    expect(idsOf(filterThreadTree(groups, [], "FIX"))).toEqual(["1"]);
  });

  it("匹配目录名（basename）时带出该目录全部会话", () => {
    const r = filterThreadTree(groups, [], "projA");
    expect(idsOf(r)).toEqual(["1"]);
  });

  it("匹配完整路径", () => {
    expect(idsOf(filterThreadTree(groups, [], "/work/projB"))).toEqual(["2"]);
  });

  it("目录命中时保留该目录下所有会话，而不只是标题也命中的", () => {
    const two: WorkspaceGroup[] = [
      {
        workspace: "/work/projA",
        exists: true,
        threads: [
          thread("1", "fix login bug", "/work/projA"),
          thread("2", "unrelated title", "/work/projA"),
        ],
      },
    ];
    expect(idsOf(filterThreadTree(two, [], "projA"))).toEqual(["1", "2"]);
  });

  it("多词 AND：两词分别命中标题与路径才保留", () => {
    expect(idsOf(filterThreadTree(groups, [], "fix projA"))).toEqual(["1"]);
    // "fix" 命中标题但 "projZ" 谁都不命中 → 全部剔除。
    expect(idsOf(filterThreadTree(groups, [], "fix projZ"))).toEqual([]);
  });

  it("title 为 null 时按 (untitled) 匹配", () => {
    const g: WorkspaceGroup[] = [
      { workspace: "/w", exists: true, threads: [thread("9", null, "/w")] },
    ];
    expect(idsOf(filterThreadTree(g, [], "untitled"))).toEqual(["9"]);
  });

  it("不搜 thread_id", () => {
    // 标题与目录都不含 "42"，只有 id 含；不得命中。
    const g: WorkspaceGroup[] = [
      { workspace: "/w", exists: true, threads: [thread("42", "hello", "/w")] },
    ];
    expect(idsOf(filterThreadTree(g, [], "42"))).toEqual([]);
  });

  it("一行不剩的分组被整组移除", () => {
    const r = filterThreadTree(groups, [], "fix");
    expect(r.groups.map((g) => g.workspace)).toEqual(["/work/projA"]);
  });

  it("分组只剩置顶行可渲染时整组移除，而置顶会话仍在 pinned 中返回", () => {
    // 置顶会话按约定仍留在各自分组内，但分组**不**渲染它（Pinned 区渲染）。
    const pin = { ...thread("p", "pin-a", "/work/projA"), pinned: true };
    const normal = thread("n", "normal thing", "/work/projA");
    const g: WorkspaceGroup[] = [
      { workspace: "/work/projA", exists: true, threads: [normal, pin] },
    ];

    // 只有置顶会话命中 ⇒ 该分组一行都渲染不出来 ⇒ 连表头一起移除。
    const hitPin = filterThreadTree(g, [pin], "pin-a");
    expect(hitPin.pinned.map((t) => t.thread_id)).toEqual(["p"]);
    expect(hitPin.groups).toEqual([]);

    // 命中普通会话时分组照常保留（意图：移除只由「零可渲染行」触发）。
    const hitNormal = filterThreadTree(g, [pin], "normal thing");
    expect(hitNormal.groups.map((w) => w.workspace)).toEqual(["/work/projA"]);
    expect(hitNormal.groups[0].threads.map((t) => t.thread_id)).toEqual(["n"]);
  });

  it("卡片会话不算「零可渲染行」：命中的卡片由分组自身的「看板会话」小节渲染", () => {
    // 与置顶会话不同：卡片会话不在分组外的独立区段渲染，而由本组的
    // renderBoardThreads 渲染（搜索期间强制展开）。故卡片命中时分组必须保留，
    // 否则 ThreadSidebar 那条「搜索命中折叠小节时自动展开」的行为会被误删。
    const card = { ...thread("c", "card-a", "/work/projB"), driver_ref: "k1" };
    const normal = thread("n", "normal thing", "/work/projB");
    const g: WorkspaceGroup[] = [
      { workspace: "/work/projB", exists: true, threads: [normal, card] },
    ];
    expect(idsOf(filterThreadTree(g, [], "card-a"))).toEqual(["c"]);
    expect(idsOf(filterThreadTree(g, [], "normal thing"))).toEqual(["n"]);
  });

  it("无命中时 groups 为空且 empty 为 true", () => {
    const r = filterThreadTree(groups, [], "zzz-nothing");
    expect(r.groups).toEqual([]);
    expect(r.empty).toBe(true);
  });

  it("pinned 用自身 cwd 参与路径匹配", () => {
    const pin = thread("9", "pinned thing", "/work/projA");
    const r = filterThreadTree(groups, [pin], "projA");
    expect(r.pinned.map((t) => t.thread_id)).toEqual(["9"]);
  });

  it("pinned 全不命中时返回空数组，且不影响 empty 判定（分组仍命中）", () => {
    const pin = thread("9", "pinned thing", "/work/projZ");
    const r = filterThreadTree(groups, [pin], "fix");
    expect(r.pinned).toEqual([]);
    expect(r.empty).toBe(false);
  });

  it("分组与 pinned 都无命中时 empty 为 true", () => {
    const pin = thread("9", "pinned thing", "/work/projZ");
    expect(filterThreadTree(groups, [pin], "zzz").empty).toBe(true);
  });

  it("matches a thread by its tag's default name", () => {
    const withTag: WorkspaceGroup[] = [
      {
        workspace: "/work/projA",
        exists: true,
        threads: [
          { thread_id: "1", cwd: "/work/projA", model: "m", created_at: 0, updated_at: 0, title: "alpha", tag: "red" },
          { thread_id: "2", cwd: "/work/projA", model: "m", created_at: 0, updated_at: 0, title: "beta", tag: "blue" },
        ],
      },
    ];
    // 默认名「红」命中，且只命中打了 red 的那条。
    expect(idsOf(filterThreadTree(withTag, [], "红"))).toEqual(["1"]);
  });

  it("matches a thread by a customized tag name, not the default", () => {
    const withTag: WorkspaceGroup[] = [
      {
        workspace: "/work/projA",
        exists: true,
        threads: [
          { thread_id: "1", cwd: "/work/projA", model: "m", created_at: 0, updated_at: 0, title: "alpha", tag: "red" },
        ],
      },
    ];
    const names = resolveTagNames({ red: "紧急" });
    expect(idsOf(filterThreadTree(withTag, [], "紧急", names))).toEqual(["1"]);
    // 改过名后旧名不再命中。
    expect(filterThreadTree(withTag, [], "红", names).empty).toBe(true);
  });

  it("ignores an invalid tag when searching", () => {
    const withTag: WorkspaceGroup[] = [
      {
        workspace: "/work/projA",
        exists: true,
        threads: [
          { thread_id: "1", cwd: "/work/projA", model: "m", created_at: 0, updated_at: 0, title: "alpha", tag: "chartreuse" },
        ],
      },
    ];
    // 脏 tag 不并入匹配文本，也不崩；按标题仍能命中。
    expect(idsOf(filterThreadTree(withTag, [], "alpha"))).toEqual(["1"]);
    expect(filterThreadTree(withTag, [], "chartreuse").empty).toBe(true);
  });
});

describe("filterThreadTree 与分组框", () => {
  const boxed = (): WorkspaceGroup[] => [
    {
      workspace: "/p",
      exists: true,
      threads: [thread("a", "登录页面", "/p"), thread("b", "扫码", "/p")],
      groups: [{ id: "g", name: "登录模块", description: "", threadIds: ["a", "b"] }],
    },
  ];

  it("搜索时组框按命中成员收窄，未命中的成员从框里消失", () => {
    const r = filterThreadTree(boxed(), [], "扫码");
    const box = (r.groups[0].groups ?? [])[0];
    // 组名本身没命中、但成员 b 命中：框仍在，成员表只剩 b。
    expect(box.threadIds).toEqual(["b"]);
  });

  it("组名命中时整组保留（组名也是可搜内容）", () => {
    const r = filterThreadTree(boxed(), [], "登录模块");
    const box = (r.groups[0].groups ?? [])[0];
    expect(box.threadIds).toEqual(["a", "b"]);
  });

  it("描述命中时整组保留", () => {
    const g = boxed();
    g[0].groups![0].description = "扫码与账密两条线";
    const r = filterThreadTree(g, [], "账密");
    expect((r.groups[0].groups ?? [])[0].threadIds).toEqual(["a", "b"]);
  });

  it("框内成员全不命中且组名/描述也不命中，整框隐藏", () => {
    const r = filterThreadTree(boxed(), [], "完全无关");
    expect(r.groups).toEqual([]);
    expect(r.empty).toBe(true);
  });

  it("框内成员全不命中、但组名命中：框与成员都保留（整组展示）", () => {
    const r = filterThreadTree(boxed(), [], "登录模块");
    expect(r.groups).toHaveLength(1);
    expect((r.groups[0].groups ?? [])[0].threadIds).toEqual(["a", "b"]);
    // 成员行也不能被查询筛空，否则展开是个空框。
    expect(idsOf(r).sort()).toEqual(["a", "b"]);
  });
});
