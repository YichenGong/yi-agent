import { beforeEach, describe, expect, it, vi } from "vitest";
import { ClientRegistry } from "./clientRegistry";
import { LOCAL_SLOT } from "./slots";

class FakeClient {
  disposed = false;
  constructor(readonly name: string) {}
  dispose() {
    this.disposed = true;
  }
}

describe("ClientRegistry", () => {
  let registry: ClientRegistry<FakeClient>;

  beforeEach(() => {
    registry = new ClientRegistry<FakeClient>();
  });

  it("starts empty with local as the active slot", () => {
    expect(registry.active()).toBe(LOCAL_SLOT);
    expect(registry.current()).toBeUndefined();
    expect(registry.keys()).toEqual([]);
  });

  it("keeps one client per slot", () => {
    const local = registry.put(LOCAL_SLOT, new FakeClient("local"));
    const remote = registry.put("proj-foo", new FakeClient("remote"));
    expect(registry.get(LOCAL_SLOT)).toBe(local);
    expect(registry.get("proj-foo")).toBe(remote);
    expect(registry.keys().sort()).toEqual(["local", "proj-foo"]);
  });

  it("follows the active slot without touching the other slots", () => {
    const local = registry.put(LOCAL_SLOT, new FakeClient("local"));
    const remote = registry.put("proj-foo", new FakeClient("remote"));
    expect(registry.current()).toBe(local);
    registry.setActive("proj-foo");
    expect(registry.current()).toBe(remote);
    expect(registry.get(LOCAL_SLOT)).toBe(local);
    // 切活动槽位不得销毁任何连接：用户只是换了视图。
    expect(local.disposed).toBe(false);
    expect(remote.disposed).toBe(false);
  });

  // 重复订阅的成因：Tauri 监听不会随连接关闭自动停，旧 transport 会与新的一起
  // 消费每条帧，流式文本于是被应用两遍。
  it("disposes the previous client when a slot is replaced", () => {
    const old = registry.put(LOCAL_SLOT, new FakeClient("old"));
    registry.put(LOCAL_SLOT, new FakeClient("new"));
    expect(old.disposed).toBe(true);
  });

  it("does not dispose a client it is asked to store twice", () => {
    const client = new FakeClient("same");
    registry.put(LOCAL_SLOT, client);
    registry.put(LOCAL_SLOT, client);
    expect(client.disposed).toBe(false);
  });

  it("disposes and forgets on remove", () => {
    const remote = registry.put("proj-foo", new FakeClient("remote"));
    registry.remove("proj-foo");
    expect(remote.disposed).toBe(true);
    expect(registry.get("proj-foo")).toBeUndefined();
    expect(registry.keys()).toEqual([]);
  });

  it("removing an unknown slot is a no-op", () => {
    expect(() => registry.remove("nobody")).not.toThrow();
  });

  // spec §5.2：build/info 与 flags/list 问远端会得到「那台机器」的答案。
  it("answers global reads from the local slot even when a remote slot is active", () => {
    const local = registry.put(LOCAL_SLOT, new FakeClient("local"));
    registry.put("proj-foo", new FakeClient("remote"));
    registry.setActive("proj-foo");
    expect(registry.forGlobalReads()).toBe(local);
    expect(registry.current()?.name).toBe("remote");
  });

  it("has no global-read client before the local slot connects", () => {
    registry.put("proj-foo", new FakeClient("remote"));
    expect(registry.forGlobalReads()).toBeUndefined();
  });

  it("disposes every client exactly once on unmount", () => {
    const local = registry.put(LOCAL_SLOT, new FakeClient("local"));
    const remote = registry.put("proj-foo", new FakeClient("remote"));
    registry.disposeAll();
    expect(local.disposed).toBe(true);
    expect(remote.disposed).toBe(true);
    expect(registry.keys()).toEqual([]);
  });

  it("clears every slot even when one dispose throws", () => {
    const boom = { dispose: vi.fn(() => { throw new Error("boom"); }) };
    registry.put(LOCAL_SLOT, boom as unknown as FakeClient);
    const remote = registry.put("proj-foo", new FakeClient("remote"));
    expect(() => registry.disposeAll()).toThrow();
    expect(remote.disposed).toBe(true);
    expect(registry.keys()).toEqual([]);
  });
});
