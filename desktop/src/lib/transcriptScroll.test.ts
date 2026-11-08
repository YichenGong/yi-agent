import { describe, expect, it } from "vitest";
import {
  TranscriptScrollCoordinator,
  type ScrollLayout,
} from "./transcriptScroll";
import type { ViewAnchor } from "./viewAnchor";

const OWNER = "host/t/0/width800";

const anchor: ViewAnchor = { itemId: "i-200", offset: -10 };

function layout(over: Partial<ScrollLayout> = {}): ScrollLayout {
  return {
    owner: OWNER,
    generation: 1,
    maxTop: 9000,
    scrollTop: 5000,
    anchor: null,
    anchorTop: null,
    ...over,
  };
}

function atRest(): TranscriptScrollCoordinator {
  const c = new TranscriptScrollCoordinator(OWNER);
  c.releaseFollow();
  c.acknowledgeWrite(5000, 9000);
  return c;
}

describe("TranscriptScrollCoordinator: reading anchor", () => {
  it("下方增高不能移动读者", () => {
    const c = atRest();
    c.requestRestore(anchor);
    // 第一次布局：锚点行真实位置已知，写回它当初的视口偏移。
    expect(
      c.planLayout(layout({ anchor, anchorTop: 4990, target: { itemId: "i-200", top: 4990 } })),
    ).toEqual({ kind: "write", top: 5000, following: false });
    c.acknowledgeWrite(5000, 9000);

    // 内容在锚点**下方**长高 -> maxTop 变大、scrollTop 不变、锚点坐标不变。
    // 这正是「图片迟到把视口顶走」的形状：整体 scrollHeight 差不能当成上方新增。
    expect(
      c.planLayout(layout({ generation: 2, maxTop: 12000, anchor, anchorTop: 4990 })),
    ).toEqual({ kind: "none" });
  });

  it("上方增高时按锚点实际坐标重新对位", () => {
    const c = atRest();
    c.requestRestore(anchor);
    c.planLayout(layout({ anchor, anchorTop: 4990, target: { itemId: "i-200", top: 4990 } }));
    c.acknowledgeWrite(5000, 9000);

    // 锚点上方多出 240px：锚点行下移到 5230，视口必须跟着下移同样的量。
    expect(
      c.planLayout(layout({ generation: 2, maxTop: 9240, anchor, anchorTop: 5230 })),
    ).toEqual({ kind: "write", top: 5240, following: false });
  });

  it("恢复请求在可容纳之前保持 pending，且不冒充已恢复", () => {
    const c = atRest();
    // 保存的偏移是 -7400，但内容还不够高（maxTop 只有 3000）。
    const deep: ViewAnchor = { itemId: "i-500", offset: -7400 };
    c.requestRestore(deep);
    // 目标行已挂载，但内容总高还容纳不下这个偏移：保持 pending。
    expect(
      c.planLayout(
        layout({
          maxTop: 3000,
          scrollTop: 3000,
          anchor: deep,
          anchorTop: 2900,
          target: { itemId: "i-500", top: 2900 },
        }),
      ),
    ).toEqual({ kind: "none" });
    expect(c.pendingRestoreId()).toBe("i-500");
    // 内容补齐后必须还能对位（10300 = 2900 + 7400）。
    expect(
      c.planLayout(
        layout({
          generation: 2,
          maxTop: 12000,
          scrollTop: 3000,
          anchor: deep,
          anchorTop: 2900,
          target: { itemId: "i-500", top: 2900 },
        }),
      ),
    ).toEqual({ kind: "write", top: 10300, following: false });
  });

  it("pending 恢复只在写入后消费一次，不每帧重复", () => {
    const c = atRest();
    c.requestRestore(anchor);
    c.planLayout(layout({ anchor, anchorTop: 4990, target: { itemId: "i-200", top: 4990 } }));
    c.acknowledgeWrite(5000, 9000);
    // 用户自己滚到别处：之后即使布局变化也不该把人拽回锚点。
    c.acceptUserScroll(2000, 9000);
    expect(
      c.planLayout(layout({ generation: 2, maxTop: 9240, anchor, anchorTop: 5230 })),
    ).toEqual({ kind: "none" });
  });

  it("空白占位区没有可视消息时不采集远处锚点", () => {
    const c = new TranscriptScrollCoordinator(OWNER);
    c.releaseFollow();
    // 视口在 spacer 里：调用方传 anchor=null。这不能被读成「贴底」。
    c.acceptLayout({
      owner: OWNER,
      generation: 1,
      maxTop: 9000,
      scrollTop: 4000,
      anchor: null,
      anchorTop: null,
    });
    expect(
      c.planLayout(layout({ generation: 2, maxTop: 9400, anchor: null, anchorTop: null })),
    ).toEqual({ kind: "none" });
  });
});

describe("TranscriptScrollCoordinator: intent", () => {
  it("程序化写到 maxTop 不能自动恢复跟随", () => {
    const c = atRest();
    expect(c.isFollowing()).toBe(false);
    // 浏览器把 scrollTop 夹到 maxTop，是布局结果，不是用户意图。
    c.acknowledgeWrite(9000, 9000);
    expect(c.isFollowing()).toBe(false);
  });

  it("用户真实回到底部才恢复跟随", () => {
    const c = atRest();
    c.acceptUserScroll(9000, 9000);
    expect(c.isFollowing()).toBe(true);
  });

  it("跟随时贴底，release 后不再干涉", () => {
    const c = new TranscriptScrollCoordinator(OWNER);
    expect(c.isFollowing()).toBe(true);
    expect(c.planLayout(layout({ maxTop: 12000, scrollTop: 9000 }))).toEqual({
      kind: "write",
      top: 12000,
      following: true,
    });
    c.acknowledgeWrite(12000, 12000);
    c.releaseFollow();
    expect(c.planLayout(layout({ generation: 2, maxTop: 14000, scrollTop: 12000 }))).toEqual({
      kind: "none",
    });
  });

  it("requestLatest 是显式回到最新，会恢复跟随", () => {
    const c = atRest();
    c.requestLatest();
    expect(c.isFollowing()).toBe(true);
    expect(c.planLayout(layout({ maxTop: 12000, scrollTop: 5000 }))).toEqual({
      kind: "write",
      top: 12000,
      following: true,
    });
  });

  it("旧 owner 的布局结果一律忽略", () => {
    const c = new TranscriptScrollCoordinator(OWNER);
    expect(c.planLayout(layout({ owner: "other/t/0/width800", maxTop: 12000 }))).toEqual({
      kind: "none",
    });
  });

  it("锚点恢复优先于贴底：入场锚点与 following 冲突时以锚点为准", () => {
    const c = new TranscriptScrollCoordinator(OWNER);
    c.requestRestore(anchor);
    expect(
      c.planLayout(layout({ anchor, anchorTop: 4990, target: { itemId: "i-200", top: 4990 } })),
    ).toEqual({ kind: "write", top: 5000, following: false });
  });

  it("目标行未挂载时不写位置（不能拿当前可视行冒充目标）", () => {
    const c = atRest();
    c.requestRestore(anchor);
    expect(
      c.planLayout(layout({ anchor: { itemId: "i-100", offset: 0 }, anchorTop: 2000 })),
    ).toEqual({ kind: "none" });
    expect(c.pendingRestoreId()).toBe("i-200");
  });

  it("同一 owner 换 generation 不丢阅读锚点", () => {
    const c = atRest();
    c.requestRestore(anchor);
    c.planLayout(layout({ anchor, anchorTop: 4990, target: { itemId: "i-200", top: 4990 } }));
    c.acknowledgeWrite(5000, 9000);
    expect(
      c.planLayout(layout({ generation: 9, maxTop: 9240, anchor, anchorTop: 5230 })),
    ).toEqual({ kind: "write", top: 5240, following: false });
  });
});

describe("TranscriptScrollCoordinator: 纠正必须用锚点自己的坐标", () => {
  it("内容长在锚点下方时不动（整体 scrollHeight 差不算上方新增）", () => {
    // Spec §4.4 第 4 条。视口顶那条（i-100）没动，持有的锚点（i-200）也没动，
    // 只是下方多了 3000px：绝对不能把 3000 当成上方位移写进 scrollTop。
    const c = atRest();
    c.requestRestore(anchor);
    c.planLayout(
      layout({
        anchor: { itemId: "i-100", offset: 0 },
        anchorTop: 2000,
        positions: { "i-100": 2000, "i-200": 4990 },
        target: { itemId: "i-200", top: 4990 },
      }),
    );
    c.acknowledgeWrite(5000, 9000);

    expect(
      c.planLayout(
        layout({
          generation: 2,
          maxTop: 12_000,
          anchor: { itemId: "i-100", offset: 0 },
          anchorTop: 2000,
          positions: { "i-100": 2000, "i-200": 4990 },
        }),
      ),
    ).toEqual({ kind: "none" });
  });

  it("优先用锚点自己的坐标，不用视口顶那条的坐标", () => {
    // 视口顶那条（i-90）在 1900，锚点（i-200）在 5230。用 i-90 的坐标减保存的
    // 偏移会算成 1910（-10 偏移），那是另一条消息的位置；正确落点是 5240。
    const c = atRest();
    c.requestRestore(anchor);
    c.planLayout(
      layout({
        anchor: { itemId: "i-90", offset: 0 },
        anchorTop: 1900,
        positions: { "i-90": 1900, "i-200": 4990 },
        target: { itemId: "i-200", top: 4990 },
      }),
    );
    c.acknowledgeWrite(5000, 9000);

    expect(
      c.planLayout(
        layout({
          generation: 2,
          maxTop: 9240,
          anchor: { itemId: "i-90", offset: 0 },
          anchorTop: 1900,
          positions: { "i-90": 1900, "i-200": 5230 },
        }),
      ),
    ).toEqual({ kind: "write", top: 5240, following: false });
  });

  it("程序化写入的回声不能被读成用户接管", () => {
    // 真实浏览器在程序化写 `scrollTop` 之后也会派发 scroll 事件。若把它读成
    // 用户滚动，`readingAnchor` 会被清空，此后上方内容长高就再也没人纠回。
    // 协调器只认 `acceptUserScroll`（调用方必须已过滤回声）。
    const c = atRest();
    c.requestRestore(anchor);
    c.planLayout(
      layout({ anchor, anchorTop: 4990, positions: { "i-200": 4990 }, target: { itemId: "i-200", top: 4990 } }),
    );
    // 写入回流只是记账，不改意图、不清锚点。
    c.acknowledgeWrite(5000, 9000);
    expect(c.readingAnchorId()).toBe("i-200");

    // 确认锚点仍在生效：上方再加 240，仍然会纠正。
    expect(
      c.planLayout(
        layout({ generation: 2, maxTop: 9240, anchor, anchorTop: 5230, positions: { "i-200": 5230 } }),
      ),
    ).toEqual({ kind: "write", top: 5240, following: false });
  });
});
