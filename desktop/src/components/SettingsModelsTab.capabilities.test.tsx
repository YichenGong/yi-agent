/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SettingsModelsTab } from "./SettingsModelsTab";
import { notifyModelCapabilitiesChanged, resetModelCapabilitiesListeners } from "../lib/modelCapabilitiesBus";
import type { ModelEntryView, ModelList } from "../lib/models";

afterEach(() => {
  cleanup();
  resetModelCapabilitiesListeners();
});

const entry = (over: Partial<ModelEntryView> = {}): ModelEntryView => ({
  name: "A",
  provider: "openai",
  api_url: "https://a/v1",
  model: "m",
  has_key: true,
  api_key_masked: "••••1234",
  api_style: "chat",
  ...over,
});

function payload(models: ModelEntryView[]): ModelList {
  return { models, default_model: "A", subagent_model: null, effective: null };
}

describe("SettingsModelsTab effort capabilities", () => {
  it("shows the probed tiers and keeps an unprobed entry clearly unknown", async () => {
    const call = vi.fn(async (method: string) =>
      method === "model/list"
        ? payload([
            entry({
              name: "A",
              reasoning_capabilities: { chat: { supported: ["low", "high"], probed_at: 1 } },
            }),
            entry({ name: "B", api_url: "https://b/v1" }),
          ])
        : {},
    );
    render(<SettingsModelsTab call={call} />);

    expect(await screen.findByText("low · high")).toBeTruthy();
    expect(screen.getByText("未探测")).toBeTruthy();
  });

  it("distinguishes an empty supported list from an unknown capability", async () => {
    const call = vi.fn(async (method: string) =>
      method === "model/list"
        ? payload([
            entry({
              name: "A",
              reasoning_capabilities: { chat: { supported: [], probed_at: 1 } },
            }),
            entry({
              name: "B",
              api_url: "https://b/v1",
              reasoning_capabilities: {
                chat: { supported: [], probed_at: 1, error: "HTTP 401" },
              },
            }),
          ])
        : {},
    );
    render(<SettingsModelsTab call={call} />);

    expect(await screen.findByText("不支持显式档位")).toBeTruthy();
    expect(screen.getByText("未探测成功")).toBeTruthy();
  });

  it("starts a probe and clears the busy state when the broadcast arrives", async () => {
    let models = payload([entry({ name: "A" })]);
    const call = vi.fn(async (method: string) => {
      if (method === "model/list") return models;
      if (method === "model/probe") {
        // 宿主立即回 started；结果稍后经通知到达。
        return { started: true };
      }
      return {};
    });
    render(<SettingsModelsTab call={call} />);
    await screen.findByText("未探测");

    fireEvent.click(screen.getByRole("button", { name: "重新探测 A" }));
    await waitFor(() => expect(call).toHaveBeenCalledWith("model/probe", { name: "A" }));
    expect(await screen.findByText("探测中…")).toBeTruthy();

    models = payload([
      entry({
        name: "A",
        reasoning_capabilities: { chat: { supported: ["low"], probed_at: 2 } },
      }),
    ]);
    notifyModelCapabilitiesChanged({
      name: "A",
      capabilities: { chat: { supported: ["low"], probed_at: 2 } },
    });

    expect(await screen.findByText("low")).toBeTruthy();
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "重新探测 A" })).toBeTruthy(),
    );
  });

  it("only offers the probed tiers in the edit form", async () => {
    const call = vi.fn(async (method: string) =>
      method === "model/list"
        ? payload([
            entry({
              name: "A",
              reasoning_capabilities: { chat: { supported: ["low", "high"], probed_at: 1 } },
            }),
          ])
        : {},
    );
    render(<SettingsModelsTab call={call} />);
    await screen.findByText("low · high");

    fireEvent.click(screen.getByRole("button", { name: "编辑" }));
    const select = screen.getByLabelText("推理档位") as HTMLSelectElement;
    const values = Array.from(select.options).map((o) => o.value);
    expect(values).toEqual(["", "low", "high"]);
  });

  it("keeps a stored tier visible even when it is no longer supported", async () => {
    const call = vi.fn(async (method: string) =>
      method === "model/list"
        ? payload([
            entry({
              name: "A",
              reasoning_effort: "max",
              reasoning_capabilities: { chat: { supported: ["low"], probed_at: 1 } },
            }),
          ])
        : {},
    );
    render(<SettingsModelsTab call={call} />);
    await screen.findByText("low");

    fireEvent.click(screen.getByRole("button", { name: "编辑" }));
    const select = screen.getByLabelText("推理档位") as HTMLSelectElement;
    expect(select.value).toBe("max");
    const option = Array.from(select.options).find((o) => o.value === "max");
    expect(option?.textContent).toContain("不支持");
  });

  it("offers all tiers for an entry that has never been probed", async () => {
    const call = vi.fn(async (method: string) =>
      method === "model/list" ? payload([entry({ name: "A" })]) : {},
    );
    render(<SettingsModelsTab call={call} />);
    await screen.findByText("未探测");

    fireEvent.click(screen.getByRole("button", { name: "编辑" }));
    const select = screen.getByLabelText("推理档位") as HTMLSelectElement;
    expect(select.options.length).toBe(1 + 7);
  });
});
