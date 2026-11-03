import { describe, it, expect } from "vitest";
import {
  remoteProfileIdForPath,
  sidebarMutation,
  type RemoteProfileRow,
} from "./remoteProfiles";

const rows: RemoteProfileRow[] = [
  { id: "proj-foo", label: "foo", remote_path: "/home/dev/foo" },
  { id: "proj-bar", label: "bar", remote_path: "/home/dev/bar" },
];

describe("remoteProfileIdForPath", () => {
  it("finds the profile by its remote path", () => {
    expect(remoteProfileIdForPath(rows, "/home/dev/bar")).toBe("proj-bar");
  });

  /**
   * 关键：绝不能按本机路径命中。远端 profile 的 `local_path` 是这台机器上的
   * 目录，把它当成远端的 key 会让「移除」删掉另一个项目的配置。
   */
  it("does not match a local path", () => {
    expect(remoteProfileIdForPath(rows, "/Users/y/proj/foo")).toBeNull();
  });

  it("returns null for an unknown path instead of guessing", () => {
    expect(remoteProfileIdForPath(rows, "/home/dev/nope")).toBeNull();
    expect(remoteProfileIdForPath([], "/home/dev/foo")).toBeNull();
  });
});

describe("sidebarMutation", () => {
  it("routes local slots to the workspace index", () => {
    expect(sidebarMutation(true)).toBe("workspace");
  });

  /**
   * 远端槽位走 `remote_remove`：`workspace/remove` 在本机索引里找不到那个路径，
   * 调用静默无效果，用户点了多次分组都不消失。
   */
  it("routes remote slots to the remote config", () => {
    expect(sidebarMutation(false)).toBe("remote");
  });
});
