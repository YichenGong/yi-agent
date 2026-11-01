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
  it("shows the default and offers no restore button when the value is unset", async () => {
    render(<SettingsRuntimeTab call={callWith([field()])} />);
    // 未设置：只显示「默认 200」，不出现「已设为」。
    expect(await screen.findByText("默认 200")).toBeTruthy();
    expect(screen.queryByText(/已设为/)).toBeNull();
    // 「恢复默认」在无需恢复时**不渲染**——12 颗禁用按钮正是原来最难看的噪声。
    expect(screen.queryByRole("button", { name: /恢复默认/ })).toBeNull();
  });

  it("marks a customised value and offers restore when a value exists", async () => {
    render(<SettingsRuntimeTab call={callWith([field({ value: 500 })])} />);
    expect(await screen.findByText("已设为 500")).toBeTruthy();
    expect(screen.queryByText("默认 200")).toBeNull();
    const restore = screen.getByRole("button", { name: /恢复默认/ }) as HTMLButtonElement;
    // 本仓库没有 @testing-library/jest-dom，故用原生 DOM property。
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

  it("disables save until something changes, and undo restores the baseline", async () => {
    render(<SettingsRuntimeTab call={callWith([field({ value: 500 })])} />);
    const input = (await screen.findByLabelText("最大轮次")) as HTMLInputElement;
    const saveBtn = screen.getByRole("button", { name: /保存/ }) as HTMLButtonElement;
    const undoBtn = screen.getByRole("button", { name: /撤销改动/ }) as HTMLButtonElement;
    // 刚加载时没有改动，两颗按钮都点不动。
    expect(saveBtn.disabled).toBe(true);
    expect(undoBtn.disabled).toBe(true);

    fireEvent.change(input, { target: { value: "800" } });
    expect(saveBtn.disabled).toBe(false);
    expect(undoBtn.disabled).toBe(false);

    fireEvent.click(undoBtn);
    expect(input.value).toBe("500");
    expect(saveBtn.disabled).toBe(true);
    expect(undoBtn.disabled).toBe(true);
  });

  it("shows the settings.json path from the server", async () => {
    render(<SettingsRuntimeTab call={callWith([field()])} />);
    expect(await screen.findByText(/\/x\/settings\.json/)).toBeTruthy();
  });

  it("renders the tui group under a Chinese heading", async () => {
    const tuiField = field({
      key: "tui.notification_method",
      group: "tui",
      label: "通知协议",
      type: "select",
      options: ["auto", "osc9", "bel"],
      default: "auto",
      description: "auto 按终端自动选择；osc9 带正文，bel 只响铃",
      min: null,
    });
    render(<SettingsRuntimeTab call={callWith([tuiField])} />);
    expect(await screen.findByText("终端通知")).toBeTruthy();
    // 绝不能漏出英文组名——那是 `GROUP_LABELS[group] ?? group` 的老问题。
    expect(screen.queryByText("tui")).toBeNull();
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
