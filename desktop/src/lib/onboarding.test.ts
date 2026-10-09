import { describe, expect, it, vi } from "vitest";
import {
  applyOnboarding,
  dismissOnboarding,
  onboardingStatus,
  reasonLabel,
  testOnboarding,
} from "./onboarding";

describe("onboardingStatus", () => {
  it("carries the host payload through", async () => {
    const rpc = vi.fn().mockResolvedValue({ needed: true, dismissed: false, reasons: ["api_key"] });
    await expect(onboardingStatus(rpc)).resolves.toEqual({
      needed: true,
      dismissed: false,
      reasons: ["api_key"],
    });
    expect(rpc).toHaveBeenCalledWith("onboarding/status", {});
  });

  it("defaults a malformed payload to 'not needed'", async () => {
    const rpc = vi.fn().mockResolvedValue({});
    await expect(onboardingStatus(rpc)).resolves.toEqual({
      needed: false,
      dismissed: false,
      reasons: [],
    });
  });
});

describe("applyOnboarding", () => {
  it("sends the four fields and reports the outcome", async () => {
    const rpc = vi.fn().mockResolvedValue({ ok: true, env_written: true, imported: false, import_error: "disk" });
    const result = await applyOnboarding(rpc, {
      provider: "openai",
      model: "gpt-4o",
      api_url: "",
      api_key: "sk-x",
    });
    expect(rpc).toHaveBeenCalledWith("onboarding/apply", {
      provider: "openai",
      model: "gpt-4o",
      api_url: "",
      api_key: "sk-x",
    });
    expect(result).toEqual({ ok: true, env_written: true, imported: false, import_error: "disk" });
  });
});

describe("testOnboarding", () => {
  it("normalises a missing reason to null", async () => {
    const rpc = vi.fn().mockResolvedValue({ ok: true });
    await expect(
      testOnboarding(rpc, { provider: "openai", model: "m", api_url: "", api_key: "k" }),
    ).resolves.toEqual({ ok: true, reason: null });
  });
});

describe("dismissOnboarding", () => {
  it("calls the dismiss method", async () => {
    const rpc = vi.fn().mockResolvedValue({ ok: true });
    await dismissOnboarding(rpc);
    expect(rpc).toHaveBeenCalledWith("onboarding/dismiss", {});
  });
});

describe("reasonLabel", () => {
  it("translates the known missing-field codes to Chinese", () => {
    expect(reasonLabel("api_key")).toBe("未设置 API 密钥");
    expect(reasonLabel("provider")).toBe("未选择 API 格式");
  });

  it("passes an unknown code through unchanged", () => {
    expect(reasonLabel("mystery")).toBe("mystery");
  });
});
