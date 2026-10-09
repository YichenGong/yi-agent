/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, fireEvent, cleanup, waitFor } from "@testing-library/react";
import { ModelPicker } from "./ModelPicker";
import type { ModelList } from "../lib/models";

afterEach(cleanup);

const catalog: ModelList = {
  models: [
    {
      name: "A",
      provider: "anthropic",
      api_url: "u",
      model: "model-a",
      has_key: true,
      api_key_masked: "••••",
    },
    {
      name: "B",
      provider: "openai",
      api_url: "u2",
      model: "model-b",
      has_key: true,
      api_key_masked: "••••",
    },
  ],
  default_model: "A",
  subagent_model: null,
  effective: null,
};

/**
 * 注入接缝：`model/list` 回清单，`thread/setModel` 回**服务端的生效模型**。
 *
 * `effective` 可覆盖，测试用它证明 UI 取的是服务端返回值，而不是拿挂载时的
 * 清单在本地推断出的名字。
 */
function seam({ fail = false, effective = "model-b" }: { fail?: boolean; effective?: string } = {}) {
  const calls: { method: string; params: unknown }[] = [];
  const call = vi.fn(async (method: string, params: unknown) => {
    calls.push({ method, params });
    if (method === "model/list") return catalog;
    if (method === "thread/setModel") {
      if (fail) throw { code: -32025, message: "model_not_found" };
      return { ok: true, model: effective };
    }
    return {};
  });
  return { call, calls };
}

describe("ModelPicker", () => {
  it("shows the resolved default model on the trigger while following the default", async () => {
    const { call } = seam();
    render(
      <ModelPicker call={call} threadId="t1" currentRef={null} currentModel="model-a" onChanged={vi.fn()} />,
    );
    const trigger = await screen.findByRole("button", { name: /模型/ });
    expect(trigger.textContent).toContain("model-a");
  });

  it("sends thread/setModel with the picked entry and reports the server's model for that thread", async () => {
    const { call, calls } = seam({ effective: "model-b" });
    const onChanged = vi.fn();
    render(
      <ModelPicker call={call} threadId="t1" currentRef={null} currentModel="model-a" onChanged={onChanged} />,
    );
    fireEvent.click(await screen.findByRole("button", { name: /模型/ }));
    fireEvent.click(screen.getByRole("menuitemradio", { name: "B" }));

    await waitFor(() =>
      expect(calls).toContainEqual({
        method: "thread/setModel",
        params: { threadId: "t1", name: "B" },
      }),
    );
    // 回调带上本次渲染的会话 id，结果不会落到切换后的别的会话头上。
    expect(onChanged).toHaveBeenCalledWith("t1", "B", "model-b");
  });

  it("uses the server-returned model, not the mount-time catalog's guess", async () => {
    // 服务端说 B 生效为 "renamed"：清单里的 "model-b" 已被别的客户端改过，
    // 本地推断会得到过时的名字。
    const { call } = seam({ effective: "renamed" });
    const onChanged = vi.fn();
    render(
      <ModelPicker call={call} threadId="t1" currentRef={null} currentModel="model-a" onChanged={onChanged} />,
    );
    fireEvent.click(await screen.findByRole("button", { name: /模型/ }));
    fireEvent.click(screen.getByRole("menuitemradio", { name: "B" }));

    await waitFor(() => expect(onChanged).toHaveBeenCalledWith("t1", "B", "renamed"));
  });

  it("clears the override (name=null) when following the global default", async () => {
    const { call, calls } = seam({ effective: "model-a" });
    const onChanged = vi.fn();
    render(
      <ModelPicker call={call} threadId="t1" currentRef="B" currentModel="model-b" onChanged={onChanged} />,
    );
    fireEvent.click(await screen.findByRole("button", { name: /模型/ }));
    fireEvent.click(screen.getByRole("menuitemradio", { name: /跟随全局默认/ }));

    await waitFor(() =>
      expect(calls).toContainEqual({
        method: "thread/setModel",
        params: { threadId: "t1", name: null },
      }),
    );
    // 跟随默认后，会话生效的模型由服务端解析给出（A → model-a）。
    expect(onChanged).toHaveBeenCalledWith("t1", null, "model-a");
  });

  it("keeps the current selection and surfaces an error when the write rejects", async () => {
    const { call } = seam({ fail: true });
    const onChanged = vi.fn();
    render(
      <ModelPicker call={call} threadId="t1" currentRef={null} currentModel="model-a" onChanged={onChanged} />,
    );
    fireEvent.click(await screen.findByRole("button", { name: /模型/ }));
    fireEvent.click(screen.getByRole("menuitemradio", { name: "B" }));

    await waitFor(() => expect(screen.getByRole("alert")).toBeTruthy());
    // 写失败不乐观更新：会话的本地模型保持不变。
    expect(onChanged).not.toHaveBeenCalled();
    // 触发按钮仍显示当前（默认）模型。
    expect(screen.getByRole("button", { name: /模型/ }).textContent).toContain("model-a");
  });
});
