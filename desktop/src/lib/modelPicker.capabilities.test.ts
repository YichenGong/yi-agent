import { describe, it, expect } from "vitest";
import { REASONING_EFFORTS, buildEffortOptions, entrySupportedEfforts, applyCapabilities } from "./modelPicker";
import type { ModelEntryView, ModelList } from "./models";

const entry = (name: string, model: string): ModelEntryView => ({
  name,
  provider: "anthropic",
  api_url: "u",
  model,
  has_key: true,
  api_key_masked: "••••",
});

const list = (over: Partial<ModelList> = {}): ModelList => ({
  models: [entry("A", "model-a")],
  default_model: "A",
  subagent_model: null,
  effective: null,
  ...over,
});

describe("buildEffortOptions with capabilities", () => {
  it("lists every tier when capabilities are unknown", () => {
    const options = buildEffortOptions(null, null, null);
    expect(options.map((o) => o.value)).toEqual([null, ...REASONING_EFFORTS]);
  });

  it("lists only the supported tiers", () => {
    const options = buildEffortOptions(null, "low", ["low", "high"]);
    expect(options.map((o) => o.value)).toEqual([null, "low", "high"]);
    expect(options[0].label).toBe("未设置（继承 low）");
  });

  it("lists only the unset entry when the model supports no explicit tier", () => {
    const options = buildEffortOptions(null, null, []);
    expect(options.map((o) => o.value)).toEqual([null]);
  });

  it("keeps an unsupported selected tier visible instead of silently dropping it", () => {
    const options = buildEffortOptions("max", "low", ["low"]);
    const selected = options.filter((o) => o.selected);
    expect(selected).toHaveLength(1);
    expect(selected[0].value).toBe("max");
    expect(selected[0].label).toContain("不支持");
  });
});

describe("entrySupportedEfforts", () => {
  const withCaps = (caps: ModelEntryView["reasoning_capabilities"], over: Partial<ModelEntryView> = {}) =>
    list({
      models: [
        {
          ...entry("A", "m"),
          provider: "openai",
          api_style: "chat",
          reasoning_capabilities: caps,
          ...over,
        },
      ],
    });

  it("reads the supported tiers for the entry the session resolves to", () => {
    expect(
      entrySupportedEfforts(withCaps({ chat: { supported: ["low"], probed_at: 1 } }), null),
    ).toEqual(["low"]);
  });

  it("returns null when the entry has no capability data at all", () => {
    expect(entrySupportedEfforts(withCaps(null), null)).toBeNull();
    expect(entrySupportedEfforts(list({ models: [] }), "A")).toBeNull();
  });

  it("returns null when the probe for the effective channel errored", () => {
    // 鉴权失败/超时不是「一档都不支持」：必须保持宽松。
    expect(
      entrySupportedEfforts(
        withCaps({ chat: { supported: [], probed_at: 1, error: "HTTP 401" } }),
        null,
      ),
    ).toBeNull();
  });

  it("treats an empty but successful list as a real conclusion", () => {
    expect(
      entrySupportedEfforts(withCaps({ chat: { supported: [], probed_at: 1 } }), null),
    ).toEqual([]);
  });

  it("prefers responses for an auto entry, like the host does", () => {
    expect(
      entrySupportedEfforts(
        withCaps(
          {
            chat: { supported: ["low"], probed_at: 1 },
            responses: { supported: ["high"], probed_at: 1 },
          },
          { api_style: null },
        ),
        null,
      ),
    ).toEqual(["high"]);
  });

  it("falls back to chat when only chat has a trustworthy result", () => {
    expect(
      entrySupportedEfforts(
        withCaps({ chat: { supported: ["low"], probed_at: 1 } }, { api_style: null }),
        null,
      ),
    ).toEqual(["low"]);
  });

  it("pins the channel when api_style is explicit", () => {
    expect(
      entrySupportedEfforts(
        withCaps(
          {
            chat: { supported: ["low"], probed_at: 1 },
            responses: { supported: ["high"], probed_at: 1 },
          },
          { api_style: "responses" },
        ),
        null,
      ),
    ).toEqual(["high"]);
  });

  it("only looks at the anthropic slot for an anthropic entry", () => {
    expect(
      entrySupportedEfforts(
        withCaps({ chat: { supported: ["low"], probed_at: 1 } }, { provider: "anthropic" }),
        null,
      ),
    ).toBeNull();
  });
});

describe("applyCapabilities", () => {
  it("replaces the entry's capabilities without touching other entries", () => {
    const catalog = list({ models: [entry("A", "a"), entry("B", "b")] });
    const next = applyCapabilities(catalog, "A", {
      anthropic: { supported: ["low"], probed_at: 2 },
    });
    expect(next.models[0].reasoning_capabilities).toEqual({
      anthropic: { supported: ["low"], probed_at: 2 },
    });
    expect(next.models[1].reasoning_capabilities).toBeUndefined();
    // 原对象不被就地修改（React 靠引用变化重渲染）。
    expect(catalog.models[0].reasoning_capabilities).toBeUndefined();
  });

  it("returns the same catalog when the entry is gone", () => {
    const catalog = list();
    expect(applyCapabilities(catalog, "nope", null)).toBe(catalog);
  });
});
