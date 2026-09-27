import { describe, expect, it } from "vitest";
import { setPermissionModeParams } from "./threadPermissionMode";

describe("setPermissionModeParams", () => {
  it("builds yolo params", () => {
    expect(setPermissionModeParams("t1", "yolo")).toEqual({ threadId: "t1", mode: "yolo" });
  });
  it("builds normal params", () => {
    expect(setPermissionModeParams("t2", "normal")).toEqual({ threadId: "t2", mode: "normal" });
  });
});
