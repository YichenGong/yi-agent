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
      method === "onboarding/apply" ? { ok: true, name: "gpt-4o" } : { ok: true },
    );
    const { onDone } = renderWizard(call);
    fireEvent.click(screen.getByRole("button", { name: /开始/ }));
    fireEvent.click(screen.getByRole("button", { name: "openai" }));
    fireEvent.click(screen.getByRole("button", { name: /下一步/ }));
    fireEvent.change(await screen.findByLabelText("API 密钥"), { target: { value: "sk-x" } });
    fireEvent.change(screen.getByLabelText("模型标识"), { target: { value: "gpt-4o" } });
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    await waitFor(() => expect(onDone).toHaveBeenCalled());
    // 收尾结果随回调上抛：单写清单成功。
    expect(onDone).toHaveBeenCalledWith({ saved: true });
    expect(call).toHaveBeenCalledWith("onboarding/apply", {
      provider: "openai",
      model: "gpt-4o",
      api_url: "https://api.openai.com/v1",
      api_key: "sk-x",
    });
  });

  it("stays on the form without dismissing when the apply fails", async () => {
    // 单写清单：失败由宿主以 RPC 错误抛出。向导内联显示，不前进、不发 dismiss。
    const call = vi.fn(async (method: string) => {
      if (method === "onboarding/apply") throw new Error("写入失败");
      return { ok: true };
    });
    const { onDone } = renderWizard(call);
    fireEvent.click(screen.getByRole("button", { name: /开始/ }));
    fireEvent.click(screen.getByRole("button", { name: "openai" }));
    fireEvent.click(screen.getByRole("button", { name: /下一步/ }));
    fireEvent.change(await screen.findByLabelText("API 密钥"), { target: { value: "sk-x" } });
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    expect(await screen.findByText("写入失败")).toBeTruthy();
    expect(onDone).not.toHaveBeenCalled();
    expect(call).not.toHaveBeenCalledWith("onboarding/dismiss", {});
  });

  it("offers 'save anyway' after a failed connection test", async () => {
    const call = vi.fn(async (method: string) => {
      if (method === "onboarding/test") return { ok: false, reason: "无法连接 API 地址" };
      return { ok: true, name: "gpt-4o" };
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
