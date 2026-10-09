import { describe, expect, it } from "vitest";
import { permissionModeFromResponse, setPermissionModeParams } from "./threadPermissionMode";

describe("setPermissionModeParams", () => {
  it("builds yolo params", () => {
    expect(setPermissionModeParams("t1", "yolo")).toEqual({ threadId: "t1", mode: "yolo" });
  });
  it("builds normal params", () => {
    expect(setPermissionModeParams("t2", "normal")).toEqual({ threadId: "t2", mode: "normal" });
  });
});

describe("permissionModeFromResponse", () => {
  it("reads a valid mode off a thread response", () => {
    expect(permissionModeFromResponse({ thread_id: "t", permission_mode: "yolo" })).toBe("yolo");
    expect(permissionModeFromResponse({ thread_id: "t", permission_mode: "normal" })).toBe("normal");
  });
  it("returns null for a missing or unknown mode (never guesses normal)", () => {
    expect(permissionModeFromResponse({ thread_id: "t" })).toBeNull();
    expect(permissionModeFromResponse({ thread_id: "t", permission_mode: "wat" })).toBeNull();
    expect(permissionModeFromResponse(null)).toBeNull();
  });
});
