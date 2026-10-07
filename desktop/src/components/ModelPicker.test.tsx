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
};

/** 注入接缝：`model/list` 回清单，`thread/setModel` 记录写入（可选失败）。 */
function seam({ fail = false }: { fail?: boolean } = {}) {
  const calls: { method: string; params: unknown }[] = [];
  const call = vi.fn(async (method: string, params: unknown) => {
    calls.push({ method, params });
    if (method === "model/list") return catalog;
    if (method === "thread/setModel") {
      if (fail) throw { code: -32025, message: "model_not_found" };
      return {};
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

  it("sends thread/setModel with the picked entry and reports the resolved model", async () => {
    const { call, calls } = seam();
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
    expect(onChanged).toHaveBeenCalledWith("B", "model-b");
  });

  it("clears the override (name=null) when following the global default", async () => {
    const { call, calls } = seam();
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
    // 跟随默认后，会话生效的模型回到全局默认条目（A）解析出的 model-a。
    expect(onChanged).toHaveBeenCalledWith(null, "model-a");
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
