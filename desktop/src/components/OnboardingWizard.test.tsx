/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { OnboardingWizard } from "./OnboardingWizard";
import type { OnboardingRpc } from "../lib/onboarding";

afterEach(cleanup);

function renderWizard(call: (method: string, params: unknown) => Promise<unknown>) {
  const onDone = vi.fn();
  // 接缝在运行时就是 OnboardingRpc 的形状；`vi.fn` 退化成非泛型函数，这里补回泛型能力
  // （与 SettingsModelsTab 的 `call as ModelRpc` 同一手法）。
  render(<OnboardingWizard call={call as OnboardingRpc} onDone={onDone} />);
  return { onDone };
}

describe("OnboardingWizard", () => {
  it("walks from welcome to provider choice to the form", async () => {
    const call = vi.fn(async () => ({ ok: true }));
    renderWizard(call);
    fireEvent.click(screen.getByRole("button", { name: /开始/ }));
    // 选 API 格式这一步
    fireEvent.click(screen.getByRole("button", { name: "openai" }));
    fireEvent.click(screen.getByRole("button", { name: /下一步/ }));
    expect(await screen.findByLabelText("API 密钥")).toBeTruthy();
  });

  it("applies the config and finishes", async () => {
    const call = vi.fn(async (method: string) =>
      method === "onboarding/apply"
        ? { ok: true, env_written: true, imported: true }
        : { ok: true },
    );
    const { onDone } = renderWizard(call);
    fireEvent.click(screen.getByRole("button", { name: /开始/ }));
    fireEvent.click(screen.getByRole("button", { name: "openai" }));
    fireEvent.click(screen.getByRole("button", { name: /下一步/ }));
    fireEvent.change(await screen.findByLabelText("API 密钥"), { target: { value: "sk-x" } });
    fireEvent.change(screen.getByLabelText("模型标识"), { target: { value: "gpt-4o" } });
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    await waitFor(() => expect(onDone).toHaveBeenCalled());
    expect(call).toHaveBeenCalledWith("onboarding/apply", {
      provider: "openai",
      model: "gpt-4o",
      api_url: "https://api.openai.com",
      api_key: "sk-x",
    });
  });

  it("offers 'save anyway' after a failed connection test", async () => {
    const call = vi.fn(async (method: string) => {
      if (method === "onboarding/test") return { ok: false, reason: "无法连接 API 地址" };
      return { ok: true, env_written: true, imported: true };
    });
    const { onDone } = renderWizard(call);
    fireEvent.click(screen.getByRole("button", { name: /开始/ }));
    fireEvent.click(screen.getByRole("button", { name: "openai" }));
    fireEvent.click(screen.getByRole("button", { name: /下一步/ }));
    fireEvent.change(await screen.findByLabelText("API 密钥"), { target: { value: "sk-x" } });
    fireEvent.change(screen.getByLabelText("模型标识"), { target: { value: "gpt-4o" } });
    fireEvent.click(screen.getByRole("button", { name: /测试连接/ }));
    expect(await screen.findByText("无法连接 API 地址")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /仍然保存/ }));
    await waitFor(() => expect(onDone).toHaveBeenCalled());
  });

  it("dismisses and finishes when the user chooses 'later'", async () => {
    const call = vi.fn(async () => ({ ok: true }));
    const { onDone } = renderWizard(call);
    fireEvent.click(screen.getByRole("button", { name: /稍后设置/ }));
    await waitFor(() => expect(onDone).toHaveBeenCalled());
    expect(call).toHaveBeenCalledWith("onboarding/dismiss", {});
  });
});
