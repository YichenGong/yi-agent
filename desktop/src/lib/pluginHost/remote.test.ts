import { describe, expect, it } from "vitest";

import {
  acceptRelease,
  bindingKey,
  decideView,
  isRemote,
  revokeOnSwitch,
  sameHost,
  type HostIdentity,
  type ViewBinding,
} from "./remote";

/**
 * 这些测试钉住的是**判定**，不是渲染。远端验收（两个真实 app-server）在
 * `yi-agent-app-server/src/server/plugin_remote_tests.rs`；两边不能互相代替：
 * 宿主判定对了但这里放行，界面照样混淆；反之亦然。
 */

const SHA_A = "a".repeat(64);
const SHA_B = "b".repeat(64);

function host(overrides: Partial<HostIdentity> = {}): HostIdentity {
  return { kind: "remote:profile-a", instanceId: "remote-profile-a", generation: 1, ...overrides };
}

function binding(overrides: Partial<ViewBinding> = {}): ViewBinding {
  return {
    instanceId: "v1",
    host: host(),
    release: { plugin: "example-counter", version: "1.0.0", target: "darwin-aarch64", sha256: SHA_A },
    activation: 1,
    view: "main",
    ...overrides,
  };
}

describe("host identity", () => {
  it("distinguishes local from remote", () => {
    expect(isRemote({ kind: "local", instanceId: "x", generation: 1 })).toBe(false);
    expect(isRemote(host())).toBe(true);
  });

  it("compares generation, not just the instance", () => {
    expect(sameHost(host(), host())).toBe(true);
    expect(sameHost(host(), host({ generation: 2 }))).toBe(false);
    expect(sameHost(host({ kind: "remote:profile-b" }), host())).toBe(false);
  });

  it("puts host, generation and full release identity into the binding key", () => {
    const a = bindingKey(binding());
    const otherHost = bindingKey(binding({ host: host({ kind: "remote:profile-b" }) }));
    const otherGen = bindingKey(binding({ host: host({ generation: 2 }) }));
    const otherSha = bindingKey(
      binding({ release: { ...binding().release, sha256: SHA_B } }),
    );
    expect(new Set([a, otherHost, otherGen, otherSha]).size).toBe(4);
  });
});

describe("cross-host display", () => {
  it("does not show a view bound to host A when host B is active", () => {
    const decision = decideView({
      binding: binding(),
      activeHost: host({ kind: "remote:profile-b", instanceId: "remote-profile-b" }),
      committed: { version: "1.0.0", sha256: SHA_A, activation: 1 },
      localUiApi: "v1",
      requiredUiApi: "v1",
      cached: true,
    });
    expect(decision.show).toBe(false);
    if (!decision.show) {
      expect(decision.reason).toBe("foreign-host");
      // 提示必须点名两边，否则用户不知道自己该做什么。
      expect(decision.detail).toContain("profile-a");
      expect(decision.detail).toContain("profile-b");
    }
  });

  it("does not show a view from a superseded generation on the same host", () => {
    const decision = decideView({
      binding: binding(),
      activeHost: host({ generation: 2 }),
      committed: { version: "1.0.0", sha256: SHA_A, activation: 1 },
      localUiApi: "v1",
      requiredUiApi: "v1",
      cached: true,
    });
    expect(decision.show).toBe(false);
    if (!decision.show) expect(decision.reason).toBe("foreign-host");
  });

  it("refuses a view whose committed release is a different digest at the same version", () => {
    const decision = decideView({
      binding: binding(),
      activeHost: host(),
      committed: { version: "1.0.0", sha256: SHA_B, activation: 1 },
      localUiApi: "v1",
      requiredUiApi: "v1",
      cached: true,
    });
    expect(decision.show).toBe(false);
    if (!decision.show) expect(decision.reason).toBe("stale-generation");
  });

  it("shows a view when host, generation and release all agree", () => {
    const decision = decideView({
      binding: binding(),
      activeHost: host(),
      committed: { version: "1.0.0", sha256: SHA_A, activation: 1 },
      localUiApi: "v1",
      requiredUiApi: "v1",
      cached: true,
    });
    expect(decision).toEqual({ show: true });
  });
});

describe("old clients and offline hosts", () => {
  it("disables only the view when the UI API is incompatible", () => {
    const decision = decideView({
      binding: binding(),
      activeHost: host(),
      committed: { version: "1.0.0", sha256: SHA_A, activation: 1 },
      localUiApi: "v1",
      requiredUiApi: "v2",
      cached: true,
    });
    expect(decision.show).toBe(false);
    if (!decision.show) {
      expect(decision.reason).toBe("incompatible-ui");
      // 必须明确"不许回退到别的 UI 版本"，否则实现者会以为降级是允许的。
      expect(decision.detail).toContain("not allowed");
    }
  });

  it("restores the view from a matching verified cache while offline", () => {
    expect(
      decideView({
        binding: binding(),
        activeHost: host(),
        committed: { version: "1.0.0", sha256: SHA_A, activation: 1 },
        localUiApi: "v1",
        requiredUiApi: "v1",
        cached: true,
      }).show,
    ).toBe(true);
  });

  it("refuses to load when offline and nothing matches the cache", () => {
    const decision = decideView({
      binding: binding(),
      activeHost: host(),
      committed: { version: "1.0.0", sha256: SHA_A, activation: 1 },
      localUiApi: "v1",
      requiredUiApi: "v1",
      cached: false,
    });
    expect(decision.show).toBe(false);
    if (!decision.show) {
      expect(decision.reason).toBe("not-cached");
      expect(decision.detail).toContain("not a fallback");
    }
  });

  it("refuses when the host reports no committed activation", () => {
    const decision = decideView({
      binding: binding(),
      activeHost: host(),
      committed: null,
      localUiApi: "v1",
      requiredUiApi: "v1",
      cached: true,
    });
    expect(decision.show).toBe(false);
    if (!decision.show) expect(decision.reason).toBe("stale-generation");
  });
});

describe("release identity", () => {
  function release(overrides: Record<string, unknown> = {}) {
    return {
      plugin: "example-counter",
      version: "1.0.0",
      target: "darwin-aarch64",
      sha256: SHA_A,
      sizeBytes: 1024,
      publishedAt: 1_700_000_000,
      expiresAt: 0,
      serial: 5,
      authenticated: true,
      transfer: { handle: "rel1_abc", kind: "host-mediated" },
      directUrl: null,
      credentials: null,
      ...overrides,
    };
  }

  it("accepts a well-formed host-mediated identity", () => {
    const accepted = acceptRelease(release());
    expect(accepted.ok).toBe(true);
  });

  it("rejects an identity that is not authenticated", () => {
    const accepted = acceptRelease(release({ authenticated: false }));
    expect(accepted.ok).toBe(false);
  });

  it("rejects a direct URL", () => {
    const accepted = acceptRelease(release({ directUrl: "https://example.com/pkg" }));
    expect(accepted.ok).toBe(false);
    if (!accepted.ok) expect(accepted.error.detail).toContain("direct URL");
  });

  it("rejects credentials", () => {
    const accepted = acceptRelease(release({ credentials: { token: "secret" } }));
    expect(accepted.ok).toBe(false);
    if (!accepted.ok) expect(accepted.error.detail).toContain("credentials");
  });

  it("rejects a non-hex digest", () => {
    const accepted = acceptRelease(release({ sha256: "not-a-digest" }));
    expect(accepted.ok).toBe(false);
    if (!accepted.ok) expect(accepted.error.detail).toContain("64-character hex");
  });

  it("rejects a release without a host-mediated handle", () => {
    const accepted = acceptRelease(release({ transfer: { handle: "x", kind: "direct" } }));
    expect(accepted.ok).toBe(false);
    if (!accepted.ok) expect(accepted.error.detail).toContain("host-mediated");
  });

  it("rejects a non-object", () => {
    expect(acceptRelease(null).ok).toBe(false);
    expect(acceptRelease("nope").ok).toBe(false);
  });
});

describe("connection switch", () => {
  it("revokes every view bound to another host and keeps the target host's views", () => {
    const a = binding({ instanceId: "va" });
    const a2 = binding({ instanceId: "va2" });
    const target = host({ kind: "remote:profile-b", instanceId: "remote-profile-b" });
    const b = binding({ instanceId: "vb", host: target });

    // 切到 B：A 的两个视图必须撤销，B 自己的留着。
    expect(revokeOnSwitch([a, a2, b], target).sort()).toEqual(["va", "va2"]);
    // 反过来切回 A：只撤 B 的。
    expect(revokeOnSwitch([a, a2, b], host())).toEqual(["vb"]);
  });

  it("revokes on a generation bump too, not only on a different host", () => {
    const old = binding({ instanceId: "v-old" });
    const revoked = revokeOnSwitch([old], host({ generation: 2 }));
    expect(revoked).toEqual(["v-old"]);
  });

  it("revokes nothing when the same host is re-selected at the same generation", () => {
    expect(revokeOnSwitch([binding()], host())).toEqual([]);
  });
});
