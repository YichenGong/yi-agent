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

  it('labels the provider field as "API 格式" in the form and shows the value in the row', async () => {
    const call = vi.fn(async (method: string, _params: unknown) =>
      method === "model/list" ? payload() : { ok: true },
    );
    render(<SettingsModelsTab call={call} />);
    await screen.findByText("••••1234");

    // 4 列版把 provider 并进「连接」格（值仍是 anthropic / openai），不再单列一栏；
    // 旧措辞「提供方」一律不再出现。
    expect(screen.getByRole("columnheader", { name: "连接" })).toBeTruthy();
    expect(screen.queryByText("提供方")).toBeNull();
    expect(screen.queryByText("API 格式")).toBeNull();

    // 新增/编辑表单的字段标签仍是「API 格式」——那里是**填值**的地方，标签要明确。
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

  it("warns when entries exist but no default is set, and can set one", async () => {
    const call = vi.fn(async (method: string, _params: unknown) =>
      method === "model/list" ? payload({ default_model: null }) : { ok: true },
    );
    render(<SettingsModelsTab call={call} />);
    // 有条目却没有默认 -> 新建会话会直接被门禁拦下，必须在这里给出出路。
    expect(await screen.findByText(/还没有设置默认模型/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /设为默认/ }));
    await waitFor(() => expect(call).toHaveBeenCalledWith("model/setDefault", { name: "A" }));
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
    // 4 列版把名称包进 `<span>`（要 `truncate` 就必须有块级/行内块容器），
    // 所以选择器落到 `td span`，仍然只命中表格、不命中下拉的 `<option>`。
    await screen.findByText("suspect", { selector: "td span" });

    // 只有 suspect 该被标出；ok（含 /v1）与 anth（anthropic）不提示。
    const badges = await screen.findAllByText("缺 /v1?");
    expect(badges).toHaveLength(1);
    // 角标只是个短提示，完整句子放在 title 里（长句会把「连接」格撑成三行）。
    expect(badges[0].getAttribute("title")).toContain("以 /v1 结尾");
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

  it("keeps the desktop row compact — four columns, buttons on one line, no fixed content width", async () => {
    // 真实 Chrome 实测过的回归形状：7 列时设置弹窗（内宽约 590px）「操作」列只剩
    // 32px，三颗按钮折成三行、行高 153px；长模型名还会把 table-auto 撑到 822px，
    // 横向溢出而面板不可横向滚动。这条用例钉住三件事：
    //   ① 列数回到 4（合并同类字段）；
    //   ② `table-fixed` + 百分比列宽（auto 布局下 max-content 会撑破容器）；
    //   ③ 操作列的按钮容器 `whitespace-nowrap flex`（宁可别处换行也不竖排按钮）。
    const call = vi.fn(async (method: string, _params: unknown) =>
      method === "model/list" ? payload() : { ok: true },
    );
    const { container } = render(<SettingsModelsTab call={call} />);
    await screen.findByText("••••1234");

    const table = container.querySelector("table")!;
    expect(table.className).toContain("table-fixed");
    const headers = Array.from(table.querySelectorAll("thead th")).map((th) => th.textContent);
    expect(headers).toEqual(["模型", "连接", "推理档位", "操作"]);

    const nameCell = table.querySelector("tbody td")!;
    expect(nameCell.className).toContain("w-[30%]");
    // 名称必须可截断：不可截断时一条长名会把该格拆成五行。
    const nameText = nameCell.querySelector("span")!;
    expect(nameText.className).toContain("truncate");
    expect(nameText.getAttribute("title")).toBe("A");

    const opCell = table.querySelector("tbody tr td:last-child")!;
    const ops = opCell.querySelector("div")!;
    expect(ops.className).toContain("whitespace-nowrap");
    expect(ops.className).toContain("flex");
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
    expect(card.textContent).toContain("连接");
    expect(card.textContent).toContain("推理档位");
    // 模型标识随标题呈现（「模型」字段在卡片里已作标题，不再重复标签）。
    expect(card.textContent).toContain("m");
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
    const badges = screen.getAllByText("缺 /v1?");
    expect(badges).toHaveLength(1);
    expect(badges[0].getAttribute("title")).toContain("以 /v1 结尾");
  });
});
