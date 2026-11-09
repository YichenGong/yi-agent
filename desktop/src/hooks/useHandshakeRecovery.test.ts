/** @vitest-environment jsdom */
import { describe, expect, it } from "vitest";

import { createHandshakeRecovery } from "./useHandshakeRecovery";
import { SideExitCoordinator } from "../lib/sideExit";
import { ClientRegistry } from "../lib/clientRegistry";
import { LOCAL_SLOT } from "../lib/slots";
import type { RpcClient } from "../lib/rpc";

/**
 * 握手与轨迹就绪的**顺序**契约。
 *
 * 真实故障是「快速重连时前一轮 initialize 的回包晚到」：它会把一条刚开始
 * 初始化的连接标成就绪，此后每个 watch 都撞 `-32010`，界面却已经放行动作按钮。
 * 判据只有两条，都在这里钉住：
 *
 * 1. `onTraceHandshakeStart` 在**第一个 await 之前**被调用（否则「开始」本身
 *    就落在了别人的 epoch 里）；
 * 2. `onTraceHandshakeReady` 在 `initialize` 成功之后、`setStatus("connected")`
 *    之前被调用，且失败路径一次也不调。
 *
 * 这里不建真实 transport：夹具只回答握手真正读到的几个方法，其余一律拒绝，
 * 这样「握手多读了一个方法」会以失败的断言暴露，而不是静默通过。
 */
function harness(options: { failInitialize?: boolean; initializeDeferred?: boolean } = {}) {
  const events: string[] = [];
  let finishInitialize: (() => void) | null = null;
  const client = {
    request: (method: string) => {
      if (method === "initialize") {
        events.push("initialize");
        if (options.failInitialize) return Promise.reject(new Error("nope"));
        if (options.initializeDeferred) {
          return new Promise((resolve) => {
            finishInitialize = () => resolve({});
          });
        }
        return Promise.resolve({});
      }
      if (method === "build/info") return Promise.resolve({ build_kind: "release" });
      if (method === "ui/settings/read") return Promise.resolve({ theme: "dark" });
      if (method === "thread/listAll") return Promise.resolve({ groups: [] });
      return Promise.reject(new Error("unexpected RPC: " + method));
    },
  } as unknown as RpcClient;

  const handshake = createHandshakeRecovery({
    sideExits: new SideExitCoordinator(() => {}),
    activeSlotRef: { current: LOCAL_SLOT },
    clientRef: { current: client },
    registryRef: { current: new ClientRegistry<RpcClient>() },
    syncPlanRef: { current: null },
    leadEnsured: { current: false },
    sess: {
      refreshWorkspaces: async () => {},
      setGroups: () => {},
      setPinned: () => {},
      currentIdRef: { current: null },
    } as never,
    prefs: {
      setRelayUrl: () => {},
      themeTouchedRef: { current: true },
      confirmedThemeRef: { current: "dark" },
      setTheme: () => {},
      setFontScaleFromServer: () => {},
      setOnboarding: () => {},
    } as never,
    store: { seed: () => {}, ids: () => [], view: () => ({ session: {} }) } as never,
    isMobile: false,
    setStatus: (status: string) => events.push(`status:${status}`),
    setTagNames: () => {},
    setCurrentError: () => {},
    setBuildKind: () => {},
    setLeadAgentEnabled: () => {},
    watchmanTouchedRef: { current: true },
    setWatchmanEnabled: () => {},
    force: () => {},
    selectThread: async () => {},
    connectRemoteTargets: async () => {},
    scheduleReconnect: () => events.push("reconnect"),
    resetAttempt: () => {},
    buildResumeHints: () => ({}),
    sideStore: { bind: () => {} } as never,
    onTraceHandshakeStart: () => {
      events.push("traceStart");
      return 1;
    },
    onTraceHandshakeReady: () => {
      events.push("traceReady");
    },
  });

  return { handshake: () => handshake(client), events, client, finish: () => finishInitialize?.() };
}

describe("握手与轨迹就绪的顺序", () => {
  it("先声明开始，成功后才宣布就绪，并落在 connected 之前", async () => {
    const { handshake, events } = harness();
    await handshake();
    expect(events.indexOf("traceStart")).toBeLessThan(events.indexOf("initialize"));
    expect(events.indexOf("traceReady")).toBeGreaterThan(events.indexOf("initialize"));
    expect(events.indexOf("traceReady")).toBeLessThan(events.indexOf("status:connected"));
  });

  it("initialize 失败时不宣布就绪，并且排队重试", async () => {
    const { handshake, events } = harness({ failInitialize: true });
    await handshake();
    expect(events).not.toContain("traceReady");
    expect(events).toContain("reconnect");
  });

  it("initialize 在途时还谈不上就绪", async () => {
    const { handshake, events, finish } = harness({ initializeDeferred: true });
    const run = handshake();
    await Promise.resolve();
    // 已发出 initialize，但回包未到：这一步不许有就绪。
    expect(events).toContain("initialize");
    expect(events).not.toContain("traceReady");
    finish();
    await run;
    expect(events).toContain("traceReady");
  });
});
