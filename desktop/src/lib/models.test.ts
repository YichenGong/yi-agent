import { describe, expect, it, vi } from "vitest";
import {
  deleteModel,
  looksMissingV1,
  importEnvModel,
  isModelNotFound,
  listModels,
  setDefaultModel,
  setSubagentModel,
  setThreadModel,
  setThreadReasoningEffort,
  testModel,
  upsertModel,
} from "./models";

describe("listModels", () => {
  it("returns the catalog payload as-is", async () => {
    const payload = {
      models: [
        {
          name: "A",
          provider: "anthropic" as const,
          api_url: "https://a",
          model: "m",
          has_key: true,
          api_key_masked: "••••1234",
        },
      ],
      default_model: "A",
      subagent_model: null,
      effective: null,
    };
    const rpc = vi.fn().mockResolvedValue(payload);
    await expect(listModels(rpc)).resolves.toEqual(payload);
    expect(rpc).toHaveBeenCalledWith("model/list", {});
  });

  it("defaults a missing models array to empty and nulls to null", async () => {
    const rpc = vi.fn().mockResolvedValue({});
    await expect(listModels(rpc)).resolves.toEqual({
      models: [],
      default_model: null,
      subagent_model: null,
      effective: null,
    });
  });
});

describe("listModels.effective", () => {
  it("carries the effective view through", async () => {
    const rpc = vi.fn().mockResolvedValue({
      models: [],
      default_model: null,
      subagent_model: null,
      effective: {
        source: "env",
        model_ref: null,
        provider: "openai",
        api_url: "https://env",
        model: "env-model",
        has_key: true,
        api_key_masked: "••••9999",
      },
    });
    const list = await listModels(rpc);
    expect(list.effective?.source).toBe("env");
    expect(list.effective?.model).toBe("env-model");
  });

  it("defaults a missing effective to null", async () => {
    const rpc = vi.fn().mockResolvedValue({ models: [], default_model: null, subagent_model: null });
    expect((await listModels(rpc)).effective).toBeNull();
  });
});

describe("importEnvModel", () => {
  it("calls model/importEnv with empty params and returns the new name", async () => {
    const rpc = vi.fn().mockResolvedValue({ ok: true, name: "env-model", default_model: "env-model" });
    const out = await importEnvModel(rpc);
    expect(rpc).toHaveBeenCalledWith("model/importEnv", {});
    expect(out).toEqual({ name: "env-model" });
  });
});

describe("upsertModel", () => {
  it("omits api_key when the caller leaves it undefined", async () => {
    const rpc = vi.fn().mockResolvedValue({ ok: true });
    await upsertModel(rpc, { name: "A", provider: "anthropic", api_url: "u", model: "m" });
    expect(rpc).toHaveBeenCalledWith("model/upsert", {
      name: "A",
      provider: "anthropic",
      api_url: "u",
      model: "m",
    });
    expect(rpc.mock.calls[0][1]).not.toHaveProperty("api_key");
  });

  it("passes api_key through when the caller supplies one", async () => {
    const rpc = vi.fn().mockResolvedValue({ ok: true });
    await upsertModel(rpc, {
      name: "A",
      provider: "openai",
      api_url: "u",
      model: "m",
      api_key: "sk-new",
    });
    expect(rpc).toHaveBeenCalledWith("model/upsert", {
      name: "A",
      provider: "openai",
      api_url: "u",
      model: "m",
      api_key: "sk-new",
    });
  });
});

describe("deleteModel / setDefaultModel / setSubagentModel", () => {
  it("deletes by name", async () => {
    const rpc = vi.fn().mockResolvedValue({ ok: true });
    await deleteModel(rpc, "A");
    expect(rpc).toHaveBeenCalledWith("model/delete", { name: "A" });
  });

  it("sets and clears the global default", async () => {
    const rpc = vi.fn().mockResolvedValue({ ok: true });
    await setDefaultModel(rpc, "A");
    expect(rpc).toHaveBeenCalledWith("model/setDefault", { name: "A" });
    await setDefaultModel(rpc, null);
    expect(rpc).toHaveBeenLastCalledWith("model/setDefault", { name: null });
  });

  it("sets and clears the subagent model", async () => {
    const rpc = vi.fn().mockResolvedValue({ ok: true });
    await setSubagentModel(rpc, "B");
    expect(rpc).toHaveBeenCalledWith("model/setSubagent", { name: "B" });
    await setSubagentModel(rpc, null);
    expect(rpc).toHaveBeenLastCalledWith("model/setSubagent", { name: null });
  });
});

describe("setThreadModel", () => {
  it("scopes the override to the thread and returns the server's effective model", async () => {
    const rpc = vi.fn().mockResolvedValue({ ok: true, model: "model-a" });
    await expect(setThreadModel(rpc, "t1", "A")).resolves.toEqual({ model: "model-a" });
    expect(rpc).toHaveBeenCalledWith("thread/setModel", { threadId: "t1", name: "A" });
  });

  it("clears the override with an explicit null name and still returns the effective model", async () => {
    const rpc = vi.fn().mockResolvedValue({ ok: true, model: "model-default" });
    await expect(setThreadModel(rpc, "t1", null)).resolves.toEqual({ model: "model-default" });
    expect(rpc).toHaveBeenCalledWith("thread/setModel", { threadId: "t1", name: null });
  });

  it("falls back to an empty model when the server omits it", async () => {
    const rpc = vi.fn().mockResolvedValue({ ok: true });
    await expect(setThreadModel(rpc, "t1", "A")).resolves.toEqual({ model: "" });
  });
});

describe("upsertModel channel fields", () => {
  it("omits api_style / reasoning_effort when the caller leaves them undefined", async () => {
    const rpc = vi.fn().mockResolvedValue({ ok: true });
    await upsertModel(rpc, { name: "A", provider: "anthropic", api_url: "u", model: "m" });
    expect(rpc.mock.calls[0][1]).not.toHaveProperty("api_style");
    expect(rpc.mock.calls[0][1]).not.toHaveProperty("reasoning_effort");
  });

  it("passes api_style / reasoning_effort through when supplied", async () => {
    const rpc = vi.fn().mockResolvedValue({ ok: true });
    await upsertModel(rpc, {
      name: "A",
      provider: "openai",
      api_url: "u",
      model: "m",
      api_style: "responses",
      reasoning_effort: "high",
    });
    expect(rpc).toHaveBeenCalledWith("model/upsert", {
      name: "A",
      provider: "openai",
      api_url: "u",
      model: "m",
      api_style: "responses",
      reasoning_effort: "high",
    });
  });
});

describe("setThreadReasoningEffort", () => {
  it("sets the override scoped to the thread and returns the applied effort", async () => {
    const rpc = vi.fn().mockResolvedValue({ ok: true, effort: "high" });
    await expect(setThreadReasoningEffort(rpc, "t1", "high")).resolves.toEqual({ effort: "high" });
    expect(rpc).toHaveBeenCalledWith("thread/setReasoningEffort", { threadId: "t1", effort: "high" });
  });

  it("clears the override with an explicit null and reports no override", async () => {
    const rpc = vi.fn().mockResolvedValue({ ok: true, effort: null });
    await expect(setThreadReasoningEffort(rpc, "t1", null)).resolves.toEqual({ effort: null });
    expect(rpc).toHaveBeenCalledWith("thread/setReasoningEffort", { threadId: "t1", effort: null });
  });
});

describe("testModel", () => {
  it("sends api_key only when explicitly provided", async () => {
    const rpc = vi.fn().mockResolvedValue({ ok: true, reason: null });

    await testModel(rpc, {
      provider: "openai",
      model: "m",
      api_url: "https://x/v1",
      api_key: "sk-draft",
    });
    expect(rpc).toHaveBeenCalledWith(
      "model/test",
      expect.objectContaining({ api_key: "sk-draft" }),
    );
  });

  it("sends name instead of api_key when no key is given", async () => {
    const rpc = vi.fn().mockResolvedValue({ ok: true, reason: null });

    await testModel(rpc, {
      provider: "openai",
      model: "m",
      api_url: "https://x/v1",
      name: "A",
    });
    const params = rpc.mock.calls[0][1] as Record<string, unknown>;
    expect(params.name).toBe("A");
    expect("api_key" in params).toBe(false);
  });

  it("treats a missing ok field as failure", async () => {
    const rpc = vi.fn().mockResolvedValue({});
    await expect(
      testModel(rpc, { provider: "openai", model: "m", api_url: "https://x/v1", name: "A" }),
    ).resolves.toEqual({ ok: false, reason: null });
  });
});

describe("isModelNotFound", () => {
  it("reads the stable data.code", () => {
    expect(isModelNotFound({ data: { code: "model_not_found" } })).toBe(true);
    expect(isModelNotFound({ data: { code: "other" } })).toBe(false);
    expect(isModelNotFound(new Error("x"))).toBe(false);
  });

  it("also recognises the textual fallback", () => {
    expect(isModelNotFound(new Error("model_not_found: nope"))).toBe(true);
  });
});

describe("looksMissingV1", () => {
  it("flags an openai-compatible url without a /v1 suffix", () => {
    expect(looksMissingV1("openai", "https://api.example.com")).toBe(true);
    expect(looksMissingV1("openai", "https://api.example.com/openai")).toBe(true);
  });

  it("accepts a url that ends with /v1, with or without a trailing slash", () => {
    expect(looksMissingV1("openai", "https://api.example.com/v1")).toBe(false);
    expect(looksMissingV1("openai", "https://api.example.com/v1/")).toBe(false);
    expect(looksMissingV1("openai", "  https://api.example.com/v1  ")).toBe(false);
  });

  // 空白 url 的语义是「用 provider 默认地址」（`import_settings_into_catalog`
  // 会补 `https://api.openai.com/v1`）。对它报警告是纯噪音：用户什么都没写错。
  it("stays silent on a blank url, which means the provider default", () => {
    expect(looksMissingV1("openai", "")).toBe(false);
    expect(looksMissingV1("openai", "   ")).toBe(false);
  });

  it("never flags a non-openai provider", () => {
    expect(looksMissingV1("anthropic", "https://api.anthropic.com")).toBe(false);
  });
});
