import { describe, expect, it, vi } from "vitest";
import {
  deleteModel,
  isModelNotFound,
  listModels,
  setDefaultModel,
  setSubagentModel,
  setThreadModel,
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
    });
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
  it("scopes the override to the thread", async () => {
    const rpc = vi.fn().mockResolvedValue({ ok: true });
    await setThreadModel(rpc, "t1", "A");
    expect(rpc).toHaveBeenCalledWith("thread/setModel", { threadId: "t1", name: "A" });
  });

  it("clears the override with an explicit null name", async () => {
    const rpc = vi.fn().mockResolvedValue({ ok: true });
    await setThreadModel(rpc, "t1", null);
    expect(rpc).toHaveBeenCalledWith("thread/setModel", { threadId: "t1", name: null });
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
