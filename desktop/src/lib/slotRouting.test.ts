import { describe, expect, it } from "vitest";

import { ownsThreadView } from "./slotRouting";

describe("ownsThreadView", () => {
  it("lets the active slot write its own view", () => {
    expect(ownsThreadView("proj-a", "proj-a", "item/delta")).toBe(true);
  });

  it("keeps another project's turn out of this view", () => {
    // 判据 17：A 跑多步任务时 B 的视图不能跟着动。
    expect(ownsThreadView("proj-a", "proj-b", "turn/started")).toBe(false);
    expect(ownsThreadView("proj-a", "local", "item/completed")).toBe(false);
  });

  it("lets the local slot write while a remote project is shown", () => {
    expect(ownsThreadView("local", "proj-a", "turn/completed")).toBe(false);
  });

  it("lets slot-agnostic notifications through from any slot", () => {
    // 主题与模型能力描述的是这台 App，不是某条会话。
    for (const slot of ["local", "proj-a", "proj-b"]) {
      expect(ownsThreadView(slot, "proj-a", "ui/settings/updated")).toBe(true);
      expect(ownsThreadView(slot, "proj-a", "model/capabilitiesChanged")).toBe(
        true,
      );
    }
  });

  it("does not treat an unknown method as global", () => {
    // 默认必须是「按槽位过滤」：新通知若默认放行，串台会悄悄回来。
    expect(ownsThreadView("proj-a", "proj-b", "some/new/notification")).toBe(
      false,
    );
  });
});
