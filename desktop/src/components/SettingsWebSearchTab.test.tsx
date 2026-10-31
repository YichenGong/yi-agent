/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SettingsWebSearchTab } from "./SettingsWebSearchTab";

afterEach(cleanup);

const baseView = {
  enabled: true,
  active_engine: "bocha",
  path: "/h/.yi-agent/websearch.json",
  engines: {
    bocha: {
      api_key_masked: "••••1234",
      has_key: true,
      base_url: "https://api.bocha.cn/v1",
      count: 25,
      freshness: "noLimit",
    },
  },
};

describe("SettingsWebSearchTab", () => {
  it("fills the form from websearch/get and never shows a plaintext key", async () => {
    const call = vi.fn(async (m: string) =>
      m === "websearch/get" ? baseView : { ok: true },
    );
    render(<SettingsWebSearchTab call={call} />);

    const key = (await screen.findByLabelText("API Key")) as HTMLInputElement;
    // 明文永不下发，输入框的占位符就是掩码。
    expect(key.value).toBe("");
    expect(key.placeholder).toBe("••••1234");
    expect((screen.getByLabelText("结果条数") as HTMLInputElement).value).toBe("25");
    expect((screen.getByLabelText("时效范围") as HTMLSelectElement).value).toBe("noLimit");
  });

  it("saves only the fields the user changed", async () => {
    const call = vi.fn(async (m: string) =>
      m === "websearch/get" ? baseView : { ok: true },
    );
    render(<SettingsWebSearchTab call={call} />);
    await screen.findByLabelText("API Key");

    fireEvent.change(screen.getByLabelText("结果条数"), { target: { value: "10" } });
    fireEvent.click(screen.getByRole("button", { name: "保存" }));

    await waitFor(() =>
      expect(call).toHaveBeenCalledWith("websearch/set", {
        engines: { bocha: { count: 10 } },
      }),
    );
  });

  it("never sends the mask back as a key", async () => {
    const call = vi.fn(async (m: string) =>
      m === "websearch/get"
        ? { ...baseView, engines: { bocha: { ...baseView.engines.bocha, has_key: false } } }
        : { ok: true },
    );
    render(<SettingsWebSearchTab call={call} />);
    await screen.findByLabelText("API Key");

    // 只改开关，不碰 key 框。
    fireEvent.click(screen.getByLabelText("启用网页搜索"));
    fireEvent.click(screen.getByRole("button", { name: "保存" }));

    await waitFor(() => expect(call).toHaveBeenCalledWith("websearch/set", { enabled: false }));
  });

  it("shows the server's message when a connection test fails", async () => {
    const call = vi.fn(async (m: string) => {
      if (m === "websearch/get") return baseView;
      if (m === "websearch/test")
        return { ok: false, message: "API Key 无效，请检查后重试", elapsed_ms: 80 };
      return { ok: true };
    });
    render(<SettingsWebSearchTab call={call} />);
    await screen.findByLabelText("API Key");

    fireEvent.click(screen.getByRole("button", { name: "测试连接" }));
    expect(await screen.findByText(/API Key 无效/)).toBeTruthy();
  });

  it("warns that search is unavailable when no key is configured", async () => {
    const call = vi.fn(async () => ({
      ...baseView,
      engines: { bocha: { ...baseView.engines.bocha, has_key: false } },
    }));
    render(<SettingsWebSearchTab call={call} />);
    expect(await screen.findByText(/未配置 API Key/)).toBeTruthy();
  });
});
