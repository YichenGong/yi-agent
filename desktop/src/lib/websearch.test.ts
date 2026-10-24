import { describe, expect, it } from "vitest";
import { parseTestResult, parseWebSearchView } from "./websearch";

describe("parseWebSearchView", () => {
  it("maps the wire shape onto camelCase for the UI", () => {
    const v = parseWebSearchView({
      enabled: true,
      active_engine: "bocha",
      path: "/h/.yi-agent/websearch.json",
      engines: {
        bocha: {
          api_key_masked: "••••1234",
          has_key: true,
          base_url: "https://api.bocha.cn/v1",
          count: 25,
          freshness: "oneDay",
        },
      },
    });
    expect(v.engines.bocha.apiKeyMasked).toBe("••••1234");
    expect(v.engines.bocha.count).toBe(25);
    expect(v.engines.bocha.freshness).toBe("oneDay");
    expect(v.path).toBe("/h/.yi-agent/websearch.json");
  });

  it("falls back to safe defaults for a malformed payload", () => {
    const v = parseWebSearchView({ engines: "nonsense" });
    expect(v.enabled).toBe(true);
    expect(v.activeEngine).toBe("bocha");
    // 缺失的引擎补一份默认视图，面板才不会因为 key 不存在而崩。
    expect(v.engines.bocha.count).toBe(25);
    expect(v.engines.bocha.freshness).toBe("noLimit");
    expect(v.engines.bocha.hasKey).toBe(false);
  });

  it("clamps an out-of-range count and an unknown freshness", () => {
    const v = parseWebSearchView({
      engines: { bocha: { count: 900, freshness: "lastDecade" } },
    });
    expect(v.engines.bocha.count).toBe(50);
    expect(v.engines.bocha.freshness).toBe("noLimit");
  });
});

describe("parseTestResult", () => {
  it("reads a success payload", () => {
    expect(parseTestResult({ ok: true, message: "连接正常", elapsed_ms: 120 })).toEqual({
      ok: true,
      message: "连接正常",
      elapsedMs: 120,
    });
  });

  it("treats a malformed payload as a failure rather than throwing", () => {
    const r = parseTestResult(null);
    expect(r.ok).toBe(false);
    expect(r.message).toContain("失败");
  });
});
