/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, fireEvent, cleanup, waitFor } from "@testing-library/react";
import { ModelPicker } from "./ModelPicker";
import { resetModelCapabilitiesListeners } from "../lib/modelCapabilitiesBus";

afterEach(() => {
  cleanup();
  resetModelCapabilitiesListeners();
});

const EMPTY = { models: [], default_model: null, subagent_model: null, effective: null };

const catalogWithCaps = {
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
};

describe("effort tiers track the model catalog", () => {
  // 冷启动时第一次读清单可能失败（宿主还没连上）。之后的每次展开都必须能自愈，
  // 否则控件会永远按「能力未知」平铺七档 —— 用户看到的就是「档位不跟着模型变」。
  it("re-reads the catalog when the effort menu opens, recovering from a failed first read", async () => {
    let attempt = 0;
    const call = vi.fn(async (method: string) => {
      if (method !== "model/list") return {};
      attempt += 1;
      if (attempt === 1) throw { code: -1, message: "not connected" };
      return catalogWithCaps;
    });
    render(
      <ModelPicker call={call} threadId="t1" currentRef="A" currentModel="m" onChanged={vi.fn()} />,
    );
    await waitFor(() => expect(attempt).toBe(1));

    fireEvent.click(await screen.findByRole("button", { name: /推理档位/ }));

    // 展开 effort 菜单就该重读一次清单，随后只列该模型支持的档位。
    await waitFor(() => expect(screen.getByRole("menuitemradio", { name: "low" })).toBeTruthy());
    expect(screen.queryByRole("menuitemradio", { name: "max" })).toBeNull();
  });

  // 设置页改了全局默认模型（或另一客户端改了）时，本会话跟随默认。
  it("picks up a changed global default when the effort menu opens", async () => {
    let current = {
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
        {
          name: "B",
          provider: "openai",
          api_url: "u2",
          model: "m2",
          has_key: true,
          api_key_masked: "••••",
          api_style: "chat",
          reasoning_capabilities: { chat: { supported: ["medium"], probed_at: 1 } },
        },
      ],
      default_model: "A",
    };
    const call = vi.fn(async (method: string) => (method === "model/list" ? current : {}));
    render(
      <ModelPicker call={call} threadId="t1" currentRef={null} currentModel="m" onChanged={vi.fn()} />,
    );
    await waitFor(() => expect(call).toHaveBeenCalledWith("model/list", {}));

    // 另一处把全局默认换成了 B（能力也不同）。
    current = { ...current, default_model: "B" };

    fireEvent.click(await screen.findByRole("button", { name: /推理档位/ }));
    await waitFor(() => expect(screen.getByRole("menuitemradio", { name: "medium" })).toBeTruthy());
    expect(screen.queryByRole("menuitemradio", { name: "low" })).toBeNull();
    expect(screen.queryByRole("menuitemradio", { name: "high" })).toBeNull();
  });
});
