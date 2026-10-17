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

  it('labels the provider field as "API 格式" in both the table and the form', async () => {
    const call = vi.fn(async (method: string, _params: unknown) =>
      method === "model/list" ? payload() : { ok: true },
    );
    render(<SettingsModelsTab call={call} />);
    await screen.findByText("••••1234");

    // 表头用「API 格式」，不再用旧措辞「提供方」。
    expect(screen.getByText("API 格式")).toBeTruthy();
    expect(screen.queryByText("提供方")).toBeNull();

    // 新增/编辑表单的字段标签同样是「API 格式」。
    fireEvent.click(screen.getByRole("button", { name: "编辑" }));
    expect(screen.getByLabelText("API 格式")).toBeTruthy();
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
    // 空清单 + env 兜底时那句「还没有配置任何模型」是假话，绝不能与生效行并存。
    expect(screen.queryByText("还没有配置任何模型")).toBeNull();
    expect(screen.getByRole("button", { name: "导入当前配置" })).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "导入当前配置" }));
    await waitFor(() => expect(call).toHaveBeenCalledWith("model/importEnv", {}));
    // 写后重读：model/list 至少被调两次（首载 + 导入后）。
    await waitFor(() => expect(callsTo(call, "model/list").length).toBeGreaterThanOrEqual(2));
  });

  it("keeps the empty-state wording when nothing is in effect either", async () => {
    const call = vi.fn(async (method: string, _params: unknown) =>
      method === "model/list"
        ? { models: [], default_model: null, subagent_model: null, effective: null }
        : { ok: true },
    );
    render(<SettingsModelsTab call={call} />);

    // 真·空清单（连兜底都没有）：这句才是对的，且没有可导入的生效行。
    expect(await screen.findByText("还没有配置任何模型")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "导入当前配置" })).toBeNull();
  });

  it("keeps the empty-state wording when the catalog is empty without an env fallback", async () => {
    const call = vi.fn(async (method: string, _params: unknown) =>
      method === "model/list"
        ? payload({
            models: [],
            default_model: null,
            effective: {
              source: "catalog",
              model_ref: "A",
              provider: "anthropic",
              api_url: "u",
              model: "m",
              has_key: true,
              api_key_masked: "••••1234",
            },
          })
        : { ok: true },
    );
    render(<SettingsModelsTab call={call} />);

    expect(await screen.findByText("还没有配置任何模型")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "导入当前配置" })).toBeNull();
  });

  it("surfaces the host error and keeps the button when the import is rejected", async () => {
    const call = vi.fn(async (method: string, _params: unknown) => {
      if (method === "model/list")
        return payload({
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
        });
      throw {
        code: -32602,
        message: "api_url must not be empty",
        data: { code: "invalid_model" },
      };
    });
    render(<SettingsModelsTab call={call} />);
    await screen.findByText(/来自 \.env/);

    fireEvent.click(screen.getByRole("button", { name: "导入当前配置" }));

    // 导入失败不能是静默的：宿主的话必须原样露出来。
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("api_url must not be empty");
    // 失败后按钮仍在：用户能修好 .env 再点一次。
    expect(screen.getByRole("button", { name: "导入当前配置" })).toBeTruthy();
    // 失败不重读：没成功过的写不该伪造一次读取。
    expect(callsTo(call, "model/list").length).toBe(1);
  });

  it("does not offer an import when the model resolves from the catalog", async () => {
    const call = vi.fn(async (method: string, _params: unknown) =>
      method === "model/list" ? payload() : { ok: true },
    );
    render(<SettingsModelsTab call={call} />);
    await screen.findByText("••••1234");
    expect(screen.queryByRole("button", { name: "导入当前配置" })).toBeNull();
  });

  it("tests with name (not api_key) when editing without changing the key", async () => {
    const call = vi.fn(async (method: string, _params: unknown) =>
      method === "model/list" ? payload() : { ok: true, reason: null },
    );
    render(<SettingsModelsTab call={call} />);
    await screen.findByText("••••1234");

    fireEvent.click(screen.getByRole("button", { name: "编辑" }));
    fireEvent.click(screen.getByRole("button", { name: "测试" }));

    await waitFor(() => expect(callsTo(call, "model/test")).toHaveLength(1));
    const params = callsTo(call, "model/test")[0][1] as Record<string, unknown>;
    expect(params.name).toBe("A");
    expect("api_key" in params).toBe(false);
    expect(await screen.findByText("连接成功")).toBeTruthy();
  });

  it("shows the server reason when the test fails", async () => {
    const call = vi.fn(async (method: string, _params: unknown) => {
      if (method === "model/list") return { models: [], default_model: null, subagent_model: null, effective: null };
      if (method === "model/test") return { ok: false, reason: "API 密钥无效或无权限" };
      return {};
    });
    render(<SettingsModelsTab call={call} />);
    fireEvent.click(await screen.findByRole("button", { name: "添加模型" }));
    fireEvent.change(screen.getByLabelText("API 地址"), { target: { value: "https://x/v1" } });
    fireEvent.click(screen.getByRole("button", { name: "测试" }));

    expect(await screen.findByText("API 密钥无效或无权限")).toBeTruthy();
  });
});

describe("SettingsModelsTab channel fields", () => {
  it("keeps the advanced section collapsed by default", async () => {
    const call = vi.fn(async (method: string, _params: unknown) =>
      method === "model/list" ? payload() : { ok: true },
    );
    render(<SettingsModelsTab call={call} />);
    await screen.findByText("••••1234");

    fireEvent.click(screen.getByRole("button", { name: "编辑" }));
    // 「推理档位」是正常字段，直接可见。
    expect(screen.getByLabelText("推理档位")).toBeTruthy();
    // 「高级」默认收起：api_style 选择器不可见。
    expect(screen.queryByLabelText("API 通道")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: /高级/ }));
    expect(screen.getByLabelText("API 通道")).toBeTruthy();
  });

  it("sends reasoning_effort and api_style when the user picks them", async () => {
    const call = vi.fn(async (method: string, _params: unknown) =>
      method === "model/list" ? payload() : { ok: true },
    );
    render(<SettingsModelsTab call={call} />);
    await screen.findByText("••••1234");

    fireEvent.click(screen.getByRole("button", { name: "编辑" }));
    fireEvent.change(screen.getByLabelText("推理档位"), { target: { value: "high" } });
    fireEvent.click(screen.getByRole("button", { name: /高级/ }));
    fireEvent.change(screen.getByLabelText("API 通道"), { target: { value: "responses" } });
    fireEvent.click(screen.getByRole("button", { name: "保存" }));

    await waitFor(() =>
      expect(call).toHaveBeenCalledWith(
        "model/upsert",
        expect.objectContaining({ reasoning_effort: "high", api_style: "responses" }),
      ),
    );
  });

  it("does not send unset fields, so editing without touching them writes nothing extra", async () => {
    const call = vi.fn(async (method: string, _params: unknown) =>
      method === "model/list" ? payload() : { ok: true },
    );
    render(<SettingsModelsTab call={call} />);
    await screen.findByText("••••1234");

    fireEvent.click(screen.getByRole("button", { name: "编辑" }));
    fireEvent.change(screen.getByLabelText("API 地址"), { target: { value: "https://b" } });
    fireEvent.click(screen.getByRole("button", { name: "保存" }));

    await waitFor(() => expect(callsTo(call, "model/upsert").length).toBe(1));
    const params = callsTo(call, "model/upsert")[0][1] as Record<string, unknown>;
    expect(params).not.toHaveProperty("api_style");
    expect(params).not.toHaveProperty("reasoning_effort");
  });

  it("hints when an openai url does not end with /v1", async () => {
    const call = vi.fn(async (method: string, _params: unknown) =>
      method === "model/list"
        ? {
            models: [
              { name: "suspect", provider: "openai", api_url: "https://llmapi.horizon.auto", model: "m", has_key: true, api_key_masked: "••••1" },
              { name: "ok", provider: "openai", api_url: "https://llmapi.horizon.auto/v1", model: "m", has_key: true, api_key_masked: "••••2" },
              { name: "anth", provider: "anthropic", api_url: "https://llmapi.horizon.auto", model: "m", has_key: true, api_key_masked: "••••3" },
            ],
            default_model: null,
            subagent_model: null,
            effective: null,
          }
        : {},
    );
    render(<SettingsModelsTab call={call} />);
    // 模型名在两处渲染（表格单元格 + 默认/子代理的下拉选项），故限定在单元格里等。
    await screen.findByText("suspect", { selector: "td" });

    // 只有 suspect 该被标出；ok（含 /v1）与 anth（anthropic）不提示。
    expect(await screen.findAllByText(/以 \/v1 结尾/)).toHaveLength(1);
  });
});

describe("SettingsModelsTab phone layout", () => {
  it("renders cards instead of a table on the phone", async () => {
    const call = vi.fn(async (method: string, _params: unknown) =>
      method === "model/list" ? payload() : { ok: true },
    );
    const { container } = render(<SettingsModelsTab call={call} isMobile />);
    await screen.findByText("••••1234");

    // 5 列表格固有 463px，390pt 的面板只有 ~390px，窄屏必须换成纵向卡片。
    expect(container.querySelector("table")).toBeNull();
    expect(container.querySelector("[data-model-card]")).toBeTruthy();
  });

  it("keeps the table on the desktop", async () => {
    const call = vi.fn(async (method: string, _params: unknown) =>
      method === "model/list" ? payload() : { ok: true },
    );
    const { container } = render(<SettingsModelsTab call={call} />);
    await screen.findByText("••••1234");
    expect(container.querySelector("table")).toBeTruthy();
    expect(container.querySelector("[data-model-card]")).toBeNull();
  });

  it("keeps the card actionable — same labels, same callback names", async () => {
    const call = vi.fn(async (method: string, _params: unknown) =>
      method === "model/list" ? payload() : { ok: true },
    );
    const { container } = render(<SettingsModelsTab call={call} isMobile />);
    await screen.findByText("••••1234");

    // 限定在卡片内查找：表格形态的 <th> 也有同样的文本，不限定就会误通过（从而
    // 让这条用例失去「卡片必须自己渲染字段标签」的效力）。
    const card = container.querySelector("[data-model-card]")!;
    expect(card).toBeTruthy();
    expect(card.textContent).toContain("API 格式");
    expect(card.textContent).toContain("模型标识");
    // 操作按钮语义不变（可点、有可访问名）；44px 触控目标由 `py-2` 实现。
    expect(screen.getByRole("button", { name: "编辑" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "删除 A" })).toBeTruthy();
  });

  it("still shows the /v1 hint on a phone card", async () => {
    const call = vi.fn(async (method: string, _params: unknown) =>
      method === "model/list"
        ? {
            models: [
              { name: "suspect", provider: "openai", api_url: "https://llmapi.horizon.auto", model: "m", has_key: true, api_key_masked: "••••1" },
            ],
            default_model: null,
            subagent_model: null,
            effective: null,
          }
        : {},
    );
    const { container } = render(<SettingsModelsTab call={call} isMobile />);
    // 等卡片出现再断言。不用 `findByText("suspect")`——模型名还会渲染成下方两个
    // 下拉的 <option>，按文本找会命中多个（表格形态的既有用例正是靠 `selector: "td"`
    // 才避开这点；卡片形态没有 td）。
    await waitFor(() => expect(container.querySelector("[data-model-card]")).toBeTruthy());
    expect(screen.getAllByText(/以 \/v1 结尾/)).toHaveLength(1);
  });
});
