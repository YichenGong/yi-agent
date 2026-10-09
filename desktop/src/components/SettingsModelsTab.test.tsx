/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SettingsModelsTab } from "./SettingsModelsTab";
import type { ModelEntryView, ModelList } from "../lib/models";

afterEach(cleanup);

const ENTRY: ModelEntryView = {
  name: "A",
  provider: "anthropic",
  api_url: "u",
  model: "m",
  has_key: true,
  api_key_masked: "••••1234",
};

function payload(overrides: Partial<ModelList> = {}): ModelList {
  return {
    models: [ENTRY],
    default_model: "A",
    subagent_model: null,
    effective: {
      source: "catalog",
      model_ref: "A",
      provider: "anthropic",
      api_url: "u",
      model: "m",
      has_key: true,
      api_key_masked: "••••1234",
    },
    ...overrides,
  };
}

/** 计数某方法被调用了几次（不关心参数）。 */
function callsTo(call: { mock: { calls: unknown[][] } }, method: string): unknown[][] {
  return call.mock.calls.filter(([m]) => m === method);
}

describe("SettingsModelsTab", () => {
  it("lists entries with a masked key and never echoes the raw key", async () => {
    const call = vi.fn(async (method: string, _params: unknown) =>
      method === "model/list" ? payload() : { ok: true },
    );
    render(<SettingsModelsTab call={call} />);

    // 只回掩码；原始 key 根本不在这条链路上。
    expect(await screen.findByText("••••1234")).toBeTruthy();
    expect(call).toHaveBeenCalledWith("model/list", {});
  });

  it("does not send api_key when the user leaves it untouched", async () => {
    const call = vi.fn(async (method: string, _params: unknown) =>
      method === "model/list" ? payload() : { ok: true },
    );
    render(<SettingsModelsTab call={call} />);
    await screen.findByText("••••1234");

    fireEvent.click(screen.getByRole("button", { name: "编辑" }));
    // 密钥框不回填掩码、更不回填原文：留空即「不改」。
    expect((screen.getByLabelText("API 密钥") as HTMLInputElement).value).toBe("");
    fireEvent.change(screen.getByLabelText("API 地址"), { target: { value: "https://b" } });
    fireEvent.click(screen.getByRole("button", { name: "保存" }));

    await waitFor(() =>
      expect(call).toHaveBeenCalledWith(
        "model/upsert",
        expect.objectContaining({ name: "A", api_url: "https://b" }),
      ),
    );
    expect(callsTo(call, "model/upsert")[0][1]).not.toHaveProperty("api_key");
  });

  it("sends the key only after the user actually edits it", async () => {
    const call = vi.fn(async (method: string, _params: unknown) =>
      method === "model/list" ? payload() : { ok: true },
    );
    render(<SettingsModelsTab call={call} />);
    await screen.findByText("••••1234");

    fireEvent.click(screen.getByRole("button", { name: "编辑" }));
    fireEvent.change(screen.getByLabelText("API 密钥"), { target: { value: "sk-new" } });
    fireEvent.click(screen.getByRole("button", { name: "保存" }));

    await waitFor(() =>
      expect(call).toHaveBeenCalledWith(
        "model/upsert",
        expect.objectContaining({ name: "A", api_key: "sk-new" }),
      ),
    );
  });

  it("refreshes from the catalog after a successful write (no optimistic UI)", async () => {
    const call = vi.fn(async (method: string, _params: unknown) =>
      method === "model/list" ? payload() : { ok: true },
    );
    render(<SettingsModelsTab call={call} />);
    await screen.findByText("••••1234");

    fireEvent.click(screen.getByRole("button", { name: "编辑" }));
    fireEvent.change(screen.getByLabelText("API 地址"), { target: { value: "https://b" } });
    fireEvent.click(screen.getByRole("button", { name: "保存" }));

    // 写成功后必须重读，而不是把本地草稿当成新事实。
    await waitFor(() => expect(callsTo(call, "model/list").length).toBe(2));
  });

  it("shows an inline error and keeps the input on failure", async () => {
    const call = vi.fn(async (method: string, _params: unknown) => {
      if (method === "model/list") return payload();
      throw new Error("boom");
    });
    render(<SettingsModelsTab call={call} />);
    await screen.findByText("••••1234");

    fireEvent.click(screen.getByRole("button", { name: "编辑" }));
    fireEvent.change(screen.getByLabelText("API 密钥"), { target: { value: "sk-typed" } });
    fireEvent.click(screen.getByRole("button", { name: "保存" }));

    expect(await screen.findByRole("alert")).toBeTruthy();
    // 失败不乐观更新：用户改的值必须还在，且显示的就是他的输入。
    expect((screen.getByLabelText("API 密钥") as HTMLInputElement).value).toBe("sk-typed");
    // 失败也不重读：草稿留在原地等用户改。
    expect(callsTo(call, "model/list").length).toBe(1);
  });

  it("adds a new model and refreshes", async () => {
    const call = vi.fn(async (method: string, _params: unknown) =>
      method === "model/list" ? { models: [], default_model: null, subagent_model: null } : { ok: true },
    );
    render(<SettingsModelsTab call={call} />);
    expect(await screen.findByText("还没有配置任何模型")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "添加模型" }));
    fireEvent.change(screen.getByLabelText("名称"), { target: { value: "New" } });
    fireEvent.change(screen.getByLabelText("API 地址"), { target: { value: "https://x" } });
    fireEvent.change(screen.getByLabelText("模型标识"), { target: { value: "gpt" } });
    fireEvent.change(screen.getByLabelText("API 密钥"), { target: { value: "sk-1" } });
    fireEvent.click(screen.getByRole("button", { name: "保存" }));

    await waitFor(() =>
      expect(call).toHaveBeenCalledWith(
        "model/upsert",
        expect.objectContaining({ name: "New", api_url: "https://x", model: "gpt", api_key: "sk-1" }),
      ),
    );
    await waitFor(() => expect(callsTo(call, "model/list").length).toBe(2));
  });

  it("deletes a model and refreshes", async () => {
    const call = vi.fn(async (method: string, _params: unknown) =>
      method === "model/list" ? payload() : { ok: true },
    );
    render(<SettingsModelsTab call={call} />);
    await screen.findByText("••••1234");

    fireEvent.click(screen.getByRole("button", { name: "删除 A" }));
    await waitFor(() => expect(call).toHaveBeenCalledWith("model/delete", { name: "A" }));
    await waitFor(() => expect(callsTo(call, "model/list").length).toBe(2));
  });

  it("sets and clears the global default", async () => {
    const call = vi.fn(async (method: string, _params: unknown) =>
      method === "model/list" ? payload({ default_model: null }) : { ok: true },
    );
    render(<SettingsModelsTab call={call} />);
    const select = await screen.findByLabelText("全局默认模型");

    fireEvent.change(select, { target: { value: "A" } });
    await waitFor(() => expect(call).toHaveBeenCalledWith("model/setDefault", { name: "A" }));

    // 「跟随全局默认」= 清除引用（null）。
    fireEvent.change(screen.getByLabelText("全局默认模型"), { target: { value: "" } });
    await waitFor(() => expect(call).toHaveBeenCalledWith("model/setDefault", { name: null }));
  });

  it("sets the subagent model and clears it", async () => {
    const call = vi.fn(async (method: string, _params: unknown) =>
      method === "model/list" ? payload({ subagent_model: "A" }) : { ok: true },
    );
    render(<SettingsModelsTab call={call} />);
    const select = await screen.findByLabelText("子 agent 模型");
    expect((select as HTMLSelectElement).value).toBe("A");

    fireEvent.change(select, { target: { value: "" } });
    await waitFor(() => expect(call).toHaveBeenCalledWith("model/setSubagent", { name: null }));
  });

  it("says so and offers an import when the model comes from .env", async () => {
    const call = vi.fn(async (method: string, _params: unknown) =>
      method === "model/list"
        ? payload({
            models: [],
            default_model: null,
            effective: {
              source: "env",
              model_ref: null,
              provider: "openai",
              api_url: "https://env",
              model: "env-model",
              has_key: true,
              api_key_masked: "••••9999",
            },
          })
        : { ok: true },
    );
    render(<SettingsModelsTab call={call} />);

    // 如实告知「在用 .env 的那个模型」，而不是干说「还没有配置任何模型」。
    expect(await screen.findByText(/来自 \.env/)).toBeTruthy();
    expect(screen.getByText(/env-model/)).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "导入当前配置" }));
    await waitFor(() => expect(call).toHaveBeenCalledWith("model/importEnv", {}));
    // 写后重读：model/list 至少被调两次（首载 + 导入后）。
    await waitFor(() => expect(callsTo(call, "model/list").length).toBeGreaterThanOrEqual(2));
  });

  it("does not offer an import when the model resolves from the catalog", async () => {
    const call = vi.fn(async (method: string, _params: unknown) =>
      method === "model/list" ? payload() : { ok: true },
    );
    render(<SettingsModelsTab call={call} />);
    await screen.findByText("••••1234");
    expect(screen.queryByRole("button", { name: "导入当前配置" })).toBeNull();
  });
});
