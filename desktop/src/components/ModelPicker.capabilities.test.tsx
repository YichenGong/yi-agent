/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, fireEvent, cleanup } from "@testing-library/react";
import { ModelPicker } from "./ModelPicker";
import { notifyModelCapabilitiesChanged } from "../lib/modelCapabilitiesBus";
import { resetModelCapabilitiesListeners } from "../lib/modelCapabilitiesBus";

afterEach(() => {
  cleanup();
  resetModelCapabilitiesListeners();
});

const EMPTY = { models: [], default_model: null, subagent_model: null, effective: null };

function picker(getCatalog: () => unknown) {
  const call = vi.fn(async (method: string) => (method === "model/list" ? getCatalog() : {}));
  const utils = render(
    <ModelPicker call={call} threadId="t1" currentRef="A" currentModel="m" onChanged={vi.fn()} />,
  );
  return { call, ...utils };
}

describe("ModelPicker effort tiers follow the model's capabilities", () => {
  it("lists every tier while the model has not been probed", async () => {
    const { call } = picker(() => ({
      ...EMPTY,
      models: [
        {
          name: "A",
          provider: "openai",
          api_url: "u",
          model: "m",
          has_key: true,
          api_key_masked: "••••",
          api_style: "chat",
        },
      ],
      default_model: "A",
    }));
    // 等挂载读取落定再展开，避免选到占位清单。
    await vi.waitFor(() => expect(call).toHaveBeenCalledWith("model/list", {}));
    fireEvent.click(await screen.findByRole("button", { name: /推理档位/ }));
    expect(screen.getByRole("menuitemradio", { name: "max" })).toBeTruthy();
  });

  it("only offers the probed tiers for the selected model", async () => {
    const { call } = picker(() => ({
      ...EMPTY,
      models: [
        {
          name: "A",
          provider: "openai",
          api_url: "u",
          model: "m",
          has_key: true,
          api_key_masked: "••••",
          api_style: "chat",
          reasoning_capabilities: { chat: { supported: ["low", "high"], probed_at: 1 } },
        },
      ],
      default_model: "A",
    }));
    await vi.waitFor(() => expect(call).toHaveBeenCalledWith("model/list", {}));
    fireEvent.click(await screen.findByRole("button", { name: /推理档位/ }));

    expect(screen.getByRole("menuitemradio", { name: "low" })).toBeTruthy();
    expect(screen.getByRole("menuitemradio", { name: "high" })).toBeTruthy();
    expect(screen.queryByRole("menuitemradio", { name: "max" })).toBeNull();
    expect(screen.queryByRole("menuitemradio", { name: "medium" })).toBeNull();
  });

  it("re-reads the catalog when a probe result is broadcast", async () => {
    // 挂载时还没有能力数据；探测完成后宿主广播，下拉应自动收窄。
    let probed = false;
    const call = vi.fn(async (method: string) => {
      if (method !== "model/list") return {};
      return {
        ...EMPTY,
        models: [
          {
            name: "A",
            provider: "openai",
            api_url: "u",
            model: "m",
            has_key: true,
            api_key_masked: "••••",
            api_style: "chat",
            ...(probed
              ? { reasoning_capabilities: { chat: { supported: ["low"], probed_at: 1 } } }
              : {}),
          },
        ],
        default_model: "A",
      };
    });
    render(
      <ModelPicker call={call} threadId="t1" currentRef="A" currentModel="m" onChanged={vi.fn()} />,
    );
    await vi.waitFor(() => expect(call).toHaveBeenCalledWith("model/list", {}));

    probed = true;
    notifyModelCapabilitiesChanged({ name: "A", capabilities: null });
    await vi.waitFor(() =>
      expect(call.mock.calls.filter(([m]) => m === "model/list")).toHaveLength(2),
    );

    fireEvent.click(await screen.findByRole("button", { name: /推理档位/ }));
    expect(screen.getByRole("menuitemradio", { name: "low" })).toBeTruthy();
    expect(screen.queryByRole("menuitemradio", { name: "max" })).toBeNull();
  });
});