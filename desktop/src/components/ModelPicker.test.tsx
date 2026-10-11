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

/** 装配之后才在设置页新增的一条：挂载时那份清单里没有它。 */
const catalogWithC: ModelList = {
  ...catalog,
  models: [
    ...catalog.models,
    {
      name: "C",
      provider: "anthropic",
      api_url: "u3",
      model: "model-c",
      has_key: true,
      api_key_masked: "••••",
    },
  ],
};

/**
 * 注入接缝：`model/list` 回清单，`thread/setModel` 回**服务端的生效模型**。
 *
 * `effective` 可覆盖，测试用它证明 UI 取的是服务端返回值，而不是拿挂载时的
 * 清单在本地推断出的名字。
 */
function seam({
  fail = false,
  effective = "model-b",
  catalog: initialCatalog = catalog,
}: {
  fail?: boolean;
  effective?: string;
  catalog?: ModelList;
} = {}) {
  const calls: { method: string; params: unknown }[] = [];
  // 可变清单：模拟「装配之后」设置页才写了一条，宿主此刻已能读到它。
  let current = initialCatalog;
  const call = vi.fn(async (method: string, params: unknown) => {
    calls.push({ method, params });
    if (method === "model/list") return current;
    if (method === "thread/setModel") {
      if (fail) throw { code: -32025, message: "model_not_found" };
      return { ok: true, model: effective };
    }
    return {};
  });
  return { call, calls, setCatalog: (next: ModelList) => (current = next) };
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

  it("caps the model label width on the phone so it shares one line with Send", async () => {
    const { call } = seam();
    render(
      <ModelPicker call={call} threadId="t1" currentRef={null} currentModel="model-a" onChanged={vi.fn()} />,
    );
    const trigger = await screen.findByRole("button", { name: /模型/ });
    // 手机端给标签一个更小的宽度上限：`max-w-[16rem]`（256px）配 ModeChip 与 `px-4`
    // 的 Send 会超过窄屏可用宽度，把 Send 挤到下一行（三行）。收到 8rem 后三件在同一
    // 行放得下，工具栏回到两行。桌面（≥768px）不匹配 `max-md:`，逐字节不变。
    expect(trigger.className).toContain("max-md:max-w-[8rem]");
  });

  it("re-reads the catalog when the menu opens, so a model added in settings shows up", async () => {
    // 挂载时清单是 [A, B]；用户随后去设置页新增了 C。下拉只在挂载时读一次的话，
    // 这里就永远看不到 C——正是用户报的「设置里加了模型，输入框里没更新」。
    const { call, setCatalog, calls } = seam({ catalog });
    render(
      <ModelPicker call={call} threadId="t1" currentRef={null} currentModel="model-a" onChanged={vi.fn()} />,
    );
    const trigger = await screen.findByRole("button", { name: /模型/ });
    expect(calls.filter((c) => c.method === "model/list").length).toBe(1);

    // 设置页写入之后，宿主已能读到含 C 的清单。
    setCatalog(catalogWithC);
    fireEvent.click(trigger);

    // 打开即重读一次清单：新增的 C 作为一项出现。
    await waitFor(() =>
      expect(screen.getByRole("menuitemradio", { name: "C" })).toBeTruthy(),
    );
    expect(calls.filter((c) => c.method === "model/list").length).toBe(2);
  });
});
