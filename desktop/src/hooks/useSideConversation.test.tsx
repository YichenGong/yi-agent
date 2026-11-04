/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, renderHook } from "@testing-library/react";
import type { RpcClient } from "../lib/rpc";
import {
  SideConversationStore,
  sideHostKey,
  type SideHostBinding,
} from "../lib/sideConversation";
import { useSideConversation } from "./useSideConversation";

/**
 * 这一组只测**生命周期接线**：哪个控制器在什么时刻被要求「可见」、什么时候被
 * 拆掉。流式、对账、清稿的语义在 `sideSync.test.ts` 里测，不在这里重测。
 *
 * 所以 `SideSyncController` 被整个替换成一个记录器：真实类会发 RPC，而我们要
 * 断言的是「有没有人调它」，用真类只会让断言绕一圈。
 */
const recorders = vi.hoisted(() => {
  return {
    instances: [] as Array<{
      binding: unknown;
      parentId: unknown;
      visible: Array<string | null>;
      disposed: boolean;
    }>,
  };
});

vi.mock("../lib/sideSync", () => {
  class FakeSideSyncController {
    readonly client: unknown;
    readonly record: {
      binding: unknown;
      parentId: unknown;
      visible: Array<string | null>;
      disposed: boolean;
    };

    constructor(deps: { binding: { client: unknown } }) {
      this.client = deps.binding.client;
      this.record = {
        binding: deps.binding,
        parentId: null,
        visible: [],
        disposed: false,
      };
      recorders.instances.push(this.record);
    }

    setVisible(parentId: string | null): void {
      this.record.parentId = parentId;
      this.record.visible.push(parentId);
    }

    dispose(): void {
      this.record.disposed = true;
    }

    accept(): void {}

    reconcile(): Promise<void> {
      return Promise.resolve();
    }
  }
  return { SideSyncController: FakeSideSyncController };
});

afterEach(() => {
  cleanup();
  recorders.instances.length = 0;
});

function client(name: string): RpcClient {
  return { request: vi.fn(async () => ({})), name } as unknown as RpcClient;
}

const SLOT = "local" as never;

/**
 * 每条宿主用**独立槽位**。
 *
 * `SideConversationStore.bind` 对「同槽位换宿主」会先 `dropHost` 旧的——那是
 * 生产语义（一个槽位只对应一台设备）。两个宿主共用一个槽位会让第二个静默顶掉
 * 第一个，测试里看起来就像「控制器不见了」。
 */
function bindHost(
  store: SideConversationStore,
  instanceId: string,
  rpc: RpcClient,
  slot: string = "local",
): SideHostBinding {
  return store.bind({
    key: sideHostKey(slot as never, instanceId),
    slot: slot as never,
    instanceId,
    client: rpc,
    supported: true,
    scope: "control",
    connected: true,
  });
}

function setup(visible: { hostKey: string; parentId: string } | null) {
  const store = new SideConversationStore();
  const hook = renderHook(
    (props: { visible: { hostKey: string; parentId: string } | null }) =>
      useSideConversation({
        store,
        onChange: vi.fn(),
        subscribeParent: vi.fn(async () => {}),
        visible: props.visible as never,
      }),
    { initialProps: { visible } },
  );
  return { store, hook };
}

describe("useSideConversation visibility", () => {
  it("opens the tail poll on a controller created while its parent is visible", () => {
    const store = new SideConversationStore();
    const a = bindHost(store, "host-a", client("a"));
    const hook = renderHook(() =>
      useSideConversation({
        store,
        onChange: vi.fn(),
        subscribeParent: vi.fn(async () => {}),
        visible: { hostKey: a.key, parentId: "p1" },
      }),
    );
    // 用户在旁支里**重连**：控制器是在「已经可见」之后才被创建出来的。新实例
    // 必须立刻继承可见性，否则尾帧丢了也没有人对账。
    act(() => {
      hook.result.current.controllerFor(a, "p1");
    });
    expect(recorders.instances).toHaveLength(1);
    expect(recorders.instances[0].parentId).toBe("p1");
  });

  it("keeps a background host's controller invisible", () => {
    const store = new SideConversationStore();
    const a = bindHost(store, "host-a", client("a"));
    const b = bindHost(store, "host-b", client("b"), "remote-b");
    const hook = renderHook(() =>
      useSideConversation({
        store,
        onChange: vi.fn(),
        subscribeParent: vi.fn(async () => {}),
        visible: { hostKey: a.key, parentId: "p1" },
      }),
    );
    act(() => {
      hook.result.current.controllerFor(a, "p1");
      hook.result.current.controllerFor(b, "p1");
    });
    // 两条都是 p1：只按 parentId 判断会把 B 也打开。B 在后台，必须关着。
    expect(recorders.instances[0].parentId).toBe("p1");
    expect(recorders.instances[1].parentId).toBeNull();
  });

  it("moves visibility between hosts with the same parent id", () => {
    const store = new SideConversationStore();
    const a = bindHost(store, "host-a", client("a"));
    const b = bindHost(store, "host-b", client("b"), "remote-b");
    const { hook } = setup({ hostKey: a.key, parentId: "p1" });
    act(() => {
      hook.result.current.controllerFor(a, "p1");
      hook.result.current.controllerFor(b, "p1");
    });
    act(() => hook.rerender({ visible: { hostKey: b.key, parentId: "p1" } }));
    expect(recorders.instances[0].parentId).toBeNull();
    expect(recorders.instances[1].parentId).toBe("p1");
  });

  it("stops polling everything when the side view closes", () => {
    const store = new SideConversationStore();
    const a = bindHost(store, "host-a", client("a"));
    const { hook } = setup({ hostKey: a.key, parentId: "p1" });
    act(() => {
      hook.result.current.controllerFor(a, "p1");
    });
    expect(recorders.instances[0].parentId).toBe("p1");
    act(() => hook.rerender({ visible: null }));
    expect(recorders.instances[0].parentId).toBeNull();
  });

  it("does not restart the timer when the same identity re-renders", () => {
    const store = new SideConversationStore();
    const a = bindHost(store, "host-a", client("a"));
    const { hook } = setup({ hostKey: a.key, parentId: "p1" });
    act(() => {
      hook.result.current.controllerFor(a, "p1");
    });
    const afterCreate = recorders.instances[0].visible.length;
    // 同一条旁支的一次重渲染（例如流式文字变化）不能重开计时器：那会让探测
    // 永远等不到 5s，尾帧也就永远补不上。
    act(() => hook.rerender({ visible: { hostKey: a.key, parentId: "p1" } }));
    expect(recorders.instances[0].visible.length).toBe(afterCreate);
  });

  it("replaces the controller when the connection behind a host changes", () => {
    const store = new SideConversationStore();
    const a = bindHost(store, "host-a", client("a"));
    const hook = renderHook(() =>
      useSideConversation({
        store,
        onChange: vi.fn(),
        subscribeParent: vi.fn(async () => {}),
        visible: { hostKey: a.key, parentId: "p1" },
      }),
    );
    act(() => {
      hook.result.current.controllerFor(a, "p1");
    });
    // 重连：同一个 hostKey，一条新客户端。
    const reconnected = bindHost(store, "host-a", client("a2"));
    act(() => {
      hook.result.current.controllerFor(reconnected, "p1");
    });
    expect(recorders.instances).toHaveLength(2);
    // 旧实例必须拆掉：它还握着一条已经断掉的连接。
    expect(recorders.instances[0].disposed).toBe(true);
    // 新实例同样要立刻继承可见性，否则重连之后尾帧探测就没了。
    expect(recorders.instances[1].parentId).toBe("p1");
  });

  it("disposes every controller on unmount", () => {
    const store = new SideConversationStore();
    const a = bindHost(store, "host-a", client("a"));
    const b = bindHost(store, "host-b", client("b"), "remote-b");
    const { hook } = setup({ hostKey: a.key, parentId: "p1" });
    act(() => {
      hook.result.current.controllerFor(a, "p1");
      hook.result.current.controllerFor(b, "p2");
    });
    hook.unmount();
    expect(recorders.instances.map((r) => r.disposed)).toEqual([true, true]);
  });

  it("routes a frame to the controller of its own host", () => {
    // 帧的归属由**连接**决定，所以必须在 hook 用的那个 store 上绑定宿主。
    const store = new SideConversationStore();
    const hook = renderHook(() =>
      useSideConversation({
        store,
        onChange: vi.fn(),
        subscribeParent: vi.fn(async () => {}),
        visible: null,
      }),
    );
    const a = bindHost(store, "host-a", client("a"));
    bindHost(store, "host-b", client("b"), "remote-b");
    // 帧只带 parent id，归属由**连接**决定：A 连接上的帧绝不能落到 B 的控制器。
    act(() => {
      hook.result.current.accept(a.client, SLOT, {
        method: "side/itemUpdated",
        params: { side_id: "s1", parent_thread_id: "p1", generation: 1, seq: 1 },
      } as never);
    });
    expect(recorders.instances).toHaveLength(1);
    expect(recorders.instances[0].binding).toBe(a);
    // 后台宿主不会被顺手打开可见性。
    expect(recorders.instances[0].parentId).toBeNull();
  });
});
