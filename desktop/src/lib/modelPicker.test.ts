import { describe, it, expect } from "vitest";
import { REASONING_EFFORTS, buildEffortOptions, buildModelOptions, entryEffortOf, overrideFromEffective } from "./modelPicker";
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

  it("does not render an entry row that reads the same as the follow-default row", () => {
    // 用户机器上的真实情形：条目显示名 == 模型串，且它就是全局默认。
    // 此时「跟随全局默认 · X」与条目行「X」是同一个模型，下拉里只应出现一行。
    const opts = buildModelOptions(
      list({ models: [entry("HORIZON-DeepSeek", "HORIZON-DeepSeek")], default_model: "HORIZON-DeepSeek" }),
      null,
      "HORIZON-DeepSeek",
    );
    const duplicated = opts.filter((o) => o.label.includes("HORIZON-DeepSeek"));
    expect(duplicated).toHaveLength(1);
    expect(opts.map((o) => o.value)).toEqual([null]);
  });

  it("marks the single remaining row as selected when the session pins the colliding entry", () => {
    // 会话显式钉在（默认）条目上：条目行被合并掉了，不能留下「一行都没选中」的空状态。
    const opts = buildModelOptions(
      list({ models: [entry("HORIZON-DeepSeek", "HORIZON-DeepSeek")], default_model: "HORIZON-DeepSeek" }),
      "HORIZON-DeepSeek",
      "HORIZON-DeepSeek",
    );
    expect(opts.map((o) => o.value)).toEqual([null]);
    expect(opts[0].selected).toBe(true);
  });

  it("still lists every entry when no row collides with the follow-default label", () => {
    // 显示名与模型串不同（常态）时不得误删条目：三行都在。
    const opts = buildModelOptions(
      list({
        models: [entry("公司-内网", "claude-sonnet-4-5"), entry("本地", "qwen3")],
        default_model: "公司-内网",
      }),
      null,
      "claude-sonnet-4-5",
    );
    expect(opts.map((o) => o.value)).toEqual([null, "公司-内网", "本地"]);
    expect(opts[0].selected).toBe(true);
  });
});

describe("REASONING_EFFORTS", () => {
  it("lists the seven tiers in order", () => {
    expect(REASONING_EFFORTS).toEqual([
      "none",
      "minimal",
      "low",
      "medium",
      "high",
      "xhigh",
      "max",
    ]);
  });
});

describe("buildEffortOptions", () => {
  it("puts the auto option first and names the tier it falls back to", () => {
    const opts = buildEffortOptions(null, "low");
    expect(opts[0]).toMatchObject({ value: null, selected: true });
    // 「自动」而不是「未设置」：这一项是「交给条目默认决定」，不是「关闭推理」。
    // 括号里是清掉覆盖后的实际落点，用户不用去别处查。
    expect(opts[0].label).toBe("自动（low）");
  });

  it("labels the auto option without parentheses when the entry declares no default", () => {
    const opts = buildEffortOptions(null, null);
    expect(opts[0].value).toBeNull();
    expect(opts[0].label).toBe("自动");
  });

  it("lists the seven tiers after the auto option", () => {
    const opts = buildEffortOptions(null, "low");
    expect(opts.map((o) => o.value)).toEqual([
      null,
      "none",
      "minimal",
      "low",
      "medium",
      "high",
      "xhigh",
      "max",
    ]);
  });

  it("marks the current value as selected", () => {
    const opts = buildEffortOptions("high", "low");
    expect(opts[0].selected).toBe(false);
    expect(opts.find((o) => o.value === "high")?.selected).toBe(true);
    expect(opts.filter((o) => o.selected)).toHaveLength(1);
  });
});

describe("overrideFromEffective", () => {
  it("reports no override when the effective value is the entry default", () => {
    // resume 上报的是**生效值**（无覆盖时即条目默认）：与条目默认相同就是「没有
    // 可区分的覆盖」，控件该显示「自动（low）」而不是把 low 打勾。
    expect(overrideFromEffective("low", "low")).toBeNull();
  });

  it("keeps an effective value that differs from the entry default", () => {
    // 生效值与条目默认不同 ⇒ 一定存在会话覆盖（resume 带的 meta 覆盖）。
    expect(overrideFromEffective("high", "low")).toBe("high");
  });

  it("keeps an effective value the entry default cannot explain", () => {
    // 条目没声明默认而生效值非空 ⇒ 只可能是会话覆盖。
    expect(overrideFromEffective("high", null)).toBe("high");
  });

  it("passes null through", () => {
    expect(overrideFromEffective(null, "low")).toBeNull();
    expect(overrideFromEffective(null, null)).toBeNull();
  });
});

describe("entryEffortOf", () => {
  it("returns the selected entry's effort", () => {
    const catalog = list({ models: [entry("A", "model-a"), entry("B", "model-b")] });
    catalog.models[1].reasoning_effort = "high";
    expect(entryEffortOf(catalog, "B")).toBe("high");
  });

  it("falls back to the catalog's default entry when following the default", () => {
    const catalog = list({ models: [entry("A", "model-a")] });
    catalog.models[0].reasoning_effort = "medium";
    expect(entryEffortOf(catalog, null)).toBe("medium");
  });

  it("returns null when the entry declares nothing or the catalog is empty", () => {
    expect(entryEffortOf(list(), "A")).toBeNull();
    expect(entryEffortOf(list({ models: [] }), null)).toBeNull();
  });
});
