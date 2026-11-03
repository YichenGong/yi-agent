import { describe, expect, it } from "vitest";
import { observationDate, observationNotice } from "./kanbanObservation";
import type { BoardCard } from "./superpowersKanbanState";

function card(extra: Partial<BoardCard> = {}): BoardCard {
  return {
    id: "c1",
    state: "running",
    progress: null,
    detail: "",
    threadId: "t1",
    originThreadId: null,
    ...extra,
  };
}

describe("observationDate", () => {
  it("带偏移的 RFC3339 能显示到分钟", () => {
    const text = observationDate("2026-10-22T10:05:00+08:00");
    expect(text).toBeTruthy();
    expect(text).toMatch(/2026/);
    expect(text).toMatch(/10/);
  });

  it("Z 结尾也认", () => {
    expect(observationDate("2026-10-22T02:05:00Z")).toBeTruthy();
  });

  it("拒绝语法合法但日期不存在的值", () => {
    // `new Date("2026-02-30")` 在 JS 里会滚到 3 月 2 日；静默滚日期等于伪造观测时间。
    expect(observationDate("2026-02-30T10:00:00Z")).toBeNull();
    expect(observationDate("2026-13-01T10:00:00Z")).toBeNull();
    expect(observationDate("2026-10-22T25:00:00Z")).toBeNull();
  });

  it("拒绝没有时区的本地时间串", () => {
    // 没有时区就无法判断这是哪一刻；猜一个会让跨时区的两台机器显示不一致。
    expect(observationDate("2026-10-22T10:05:00")).toBeNull();
  });

  it("坏输入一律 null，不返回 Invalid Date", () => {
    expect(observationDate(undefined)).toBeNull();
    expect(observationDate("")).toBeNull();
    expect(observationDate("not a date")).toBeNull();
    expect(observationDate("2026-10-22")).toBeNull();
  });
});

describe("observationNotice", () => {
  it("已加载的卡没有角标", () => {
    expect(observationNotice(card({ sessionObservation: "loaded" }))).toBeNull();
  });

  it("明确未加载：warning 文案，绝不声称已删除", () => {
    const notice = observationNotice(
      card({ sessionObservation: "not_loaded", observedAt: "2026-10-22T10:00:00+08:00" }),
    );
    expect(notice).not.toBeNull();
    expect(notice!.label).toBe("会话未加载");
    expect(notice!.tone).toBe("warning");
    // 必须**明确否定**删除，而不是换个说法暗示。
    expect(notice!.title).toMatch(/这不是「会话已删除」/);
    expect(notice!.title).toMatch(/历史可能/);
  });

  it("无观测证据：neutral 文案，与「未加载」分开说", () => {
    const notice = observationNotice(card({ sessionObservation: "unknown" }));
    expect(notice).not.toBeNull();
    expect(notice!.label).toBe("会话观测未知");
    expect(notice!.tone).toBe("neutral");
  });

  it("旧数据的 stale 布尔仍报「未加载」，但措辞不再说已不在", () => {
    // 旧插件不发 session_observation，只给 stale:true。那是它当时对「宿主说没了」
    // 的转述，属于明确证据，所以走 warning，而不是 unknown。
    const notice = observationNotice(card({ stale: true, lastAliveAt: "2026-10-22T10:00:00+08:00" }));
    expect(notice).not.toBeNull();
    expect(notice!.label).toBe("会话未加载");
    expect(notice!.tone).toBe("warning");
    expect(notice!.title).toMatch(/这不是「会话已删除」/);
  });

  it("新字段优先于旧 stale 布尔", () => {
    // 同一份数据两个来源都在时，新字段是真相：旧布尔可能只是上一拍的残留。
    const loaded = observationNotice(card({ stale: true, sessionObservation: "loaded" }));
    expect(loaded).toBeNull();
  });

  it("什么证据都没有时不加噪音角标", () => {
    expect(observationNotice(card())).toBeNull();
  });

  it("缺 sessionObservation 的键不等于 unknown", () => {
    // 旧插件：缺键 + stale:false => 没有加载问题，不该显示「未知」。
    const notice = observationNotice(card({ stale: false }));
    expect(notice).toBeNull();
  });
});
