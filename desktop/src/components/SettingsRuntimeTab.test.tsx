/** @vitest-environment jsdom */
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  GROUP_HINTS,
  GROUP_LABELS,
  GROUP_ORDER,
  SettingsRuntimeTab,
  labelFor,
} from "./SettingsRuntimeTab";

afterEach(cleanup);

const field = (over: Partial<Record<string, unknown>> = {}) => ({
  key: "agent.max_turns",
  group: "agent",
  label: "最大轮次",
  type: "number",
  value: null,
  default: "200",
  description: "一次对话里 agent 最多执行的轮数",
  options: [],
  min: 1,
  max: null,
  ...over,
});

function callWith(fields: unknown[]) {
  return vi.fn(async (method: string) => {
    if (method === "settings/read") return { fields, path: "/x/settings.json" };
    return { ok: true };
  });
}

describe("SettingsRuntimeTab", () => {
  it("shows the default and disables restore when the value is unset", async () => {
    render(<SettingsRuntimeTab call={callWith([field()])} />);
    expect(await screen.findByText(/未设置/)).toBeTruthy();
    expect(screen.getByText(/默认 200/)).toBeTruthy();
    const restore = screen.getByRole("button", { name: /恢复默认/ }) as HTMLButtonElement;
    // 用原生 DOM property 断言。本仓库没有装 `@testing-library/jest-dom`
    // （desktop/package.json 只有 @testing-library/react），所以 `toBeDisabled()`
    // 与 `toHaveProperty()` 这类匹配器都不可用——前者没注册，后者不是 vitest 内建。
    expect(restore.disabled).toBe(true);
  });

  it("enables restore and shows the set value when a value exists", async () => {
    render(<SettingsRuntimeTab call={callWith([field({ value: 500 })])} />);
    expect(await screen.findByText(/已设为 500/)).toBeTruthy();
    const restore = screen.getByRole("button", { name: /恢复默认/ }) as HTMLButtonElement;
    expect(restore.disabled).toBe(false);
  });

  it("sends null to restore the default", async () => {
    const call = callWith([field({ value: 500 })]);
    render(<SettingsRuntimeTab call={call} />);
    fireEvent.click(await screen.findByRole("button", { name: /恢复默认/ }));
    await waitFor(() => {
      expect(call).toHaveBeenCalledWith("settings/write", {
        patch: { "agent.max_turns": null },
      });
    });
  });

  it("reports the server's rejection inline instead of a whole-page error", async () => {
    const call = vi.fn(async (method: string) => {
      if (method === "settings/read") return { fields: [field({ value: 500 })] };
      throw new Error("invalid setting agent.max_turns: must be at least 1");
    });
    render(<SettingsRuntimeTab call={call} />);
    const input = await screen.findByLabelText("最大轮次");
    fireEvent.change(input, { target: { value: "0" } });
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    expect(await screen.findByText(/must be at least 1/)).toBeTruthy();
  });
});

  it("renders an RpcError object as its message, not [object Object]", async () => {
    // RPC 失败时 reject 的是 `{code, message, data?}` 对象（lib/rpc.ts 的
    // `pending.reject(msg.error)`）。`String(obj)` 得 "[object Object]"。
    const call = vi.fn(async (method: string) => {
      if (method === "settings/read") return { fields: [field({ value: 500 })] };
      throw { code: -32602, message: "invalid setting agent.max_turns: must be at least 1" };
    });
    render(<SettingsRuntimeTab call={call} />);
    const input = await screen.findByLabelText("最大轮次");
    fireEvent.change(input, { target: { value: "0" } });
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    expect(await screen.findByText(/must be at least 1/)).toBeTruthy();
    expect(screen.queryByText(/\[object Object\]/)).toBeNull();
  });

describe("runtime settings labels", () => {
  it("names every backend group in Chinese", () => {
    // 与 yi-agent-runtime/src/settings.rs 的 FIELDS 分组一一对应。漏一个就会在页面上
    // 露出英文组名（tui 曾经就是这样）。
    expect(GROUP_ORDER).toEqual(["agent", "compact", "tui", "startup"]);
    for (const group of GROUP_ORDER) {
      expect(GROUP_LABELS[group]).toBeTruthy();
      expect(GROUP_HINTS[group]).toBeTruthy();
    }
  });

  it("translates enum values but keeps unknown ones verbatim", () => {
    expect(labelFor("startup.sandbox", "workspace-write")).toBe("工作区可写");
    expect(labelFor("tui.notification_method", "bel")).toBe("响铃");
    // 后端将来加选项时，前端不该留空或抛错。
    expect(labelFor("startup.sandbox", "some-future-mode")).toBe("some-future-mode");
    // 没有映射表的字段（数字）原样返回。
    expect(labelFor("agent.max_turns", "200")).toBe("200");
    // `default_display` 可能是空串（agent.system_prompt），显示成破折号。
    expect(labelFor("agent.system_prompt", "")).toBe("—");
  });
});
