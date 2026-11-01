import { describe, it, expect } from "vitest";
import { threadStartParams, usesLocalPaths } from "./threadStart";
import { LOCAL_SLOT } from "./slots";

describe("threadStartParams", () => {
  it("omits cwd when not provided", () => {
    expect(threadStartParams()).toEqual({});
  });

  it("includes cwd when provided", () => {
    expect(threadStartParams("/tmp/x")).toEqual({ cwd: "/tmp/x" });
  });

  it("still sends cwd for the local slot explicitly", () => {
    expect(threadStartParams("/Users/y/proj/foo", LOCAL_SLOT)).toEqual({
      cwd: "/Users/y/proj/foo",
    });
  });

  /**
   * 本机路径送到远端 `canonicalize` 必然失败。省略后服务端退回 `cfg.workdir`，
   * 即 ssh 会话自己的工作目录。
   */
  it("omits cwd for a remote slot so the remote does not canonicalize a local path", () => {
    const params = threadStartParams("/Users/y/proj/foo", "proj-foo");
    expect("cwd" in params).toBe(false);
    expect(params).toEqual({});
  });

  it("omits cwd for a remote slot even when the path looks remote", () => {
    // 判断依据是槽位，不是路径形状：任何在远端机器上「看起来对」的猜测
    // 都是猜测，远端自己知道得更清楚。
    expect(threadStartParams("/home/me/proj", "proj-foo")).toEqual({});
  });
});

describe("usesLocalPaths", () => {
  it("is true only for the local slot", () => {
    expect(usesLocalPaths(LOCAL_SLOT)).toBe(true);
    expect(usesLocalPaths("proj-foo")).toBe(false);
    // 空槽位不是本地：fail-closed 到「不要碰本机路径」。
    expect(usesLocalPaths("")).toBe(false);
  });
});
