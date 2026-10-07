import { describe, it, expect } from "vitest";
import { buildModelOptions } from "./modelPicker";
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
  ...over,
});

describe("buildModelOptions", () => {
  it("puts a follow-default option first, showing the resolved model name", () => {
    const opts = buildModelOptions(list(), null, "model-a");
    expect(opts[0]).toMatchObject({ value: null });
    expect(opts[0].label).toContain("model-a");
    expect(opts[0].selected).toBe(true);
  });

  it("falls back to the default entry's model when no session model is known", () => {
    // 会话当前模型未知（还没读到 info.model）时，仍要显示全局默认解析出的模型名。
    const opts = buildModelOptions(list(), null, null);
    expect(opts[0].label).toContain("model-a");
  });

  it("marks the session's current entry as selected", () => {
    const opts = buildModelOptions(
      list({ models: [entry("A", "model-a"), entry("B", "model-b")] }),
      "B",
      "model-b",
    );
    expect(opts[0].selected).toBe(false);
    expect(opts.find((o) => o.value === "B")?.selected).toBe(true);
    expect(opts.find((o) => o.value === "A")?.selected).toBe(false);
  });

  it("lists every model entry after the follow-default option", () => {
    const opts = buildModelOptions(
      list({ models: [entry("A", "model-a"), entry("B", "model-b")] }),
      null,
      "model-a",
    );
    expect(opts.map((o) => o.value)).toEqual([null, "A", "B"]);
  });

  it("keeps the follow-default option usable when no default is configured", () => {
    const opts = buildModelOptions(list({ default_model: null }), null, null);
    expect(opts[0].value).toBeNull();
    expect(opts[0].label).toContain("跟随全局默认");
  });

  it("does not leak the session override's model into the follow-default label", () => {
    // 会话覆盖到 B 时，「跟随全局默认」应显示全局默认（A）的模型，而不是 B 的。
    const opts = buildModelOptions(
      list({ models: [entry("A", "model-a"), entry("B", "model-b")] }),
      "B",
      "model-b",
    );
    expect(opts[0].label).toContain("model-a");
    expect(opts[0].label).not.toContain("model-b");
  });
});
