/** @vitest-environment jsdom */
import { afterEach, describe, expect, it } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import type { PluginRpc } from "../lib/pluginSettings";
import { SuperpowersKanbanPluginSettings } from "./SuperpowersKanbanPluginSettings";

afterEach(cleanup);

const EMPTY_SETTINGS = { default_max_tasks: 3, interval_secs: 10, windows: [] };

/** 每次调用都返回一个新的函数身份，模拟宿主每次渲染换新接缝。 */
function makeRpc(onRead: () => void): PluginRpc {
  return async <T,>(method: string): Promise<T> => {
    if (method === "plugin/settings/read") {
      onRead();
      return { settings: EMPTY_SETTINGS } as T;
    }
    return undefined as T;
  };
}

/** 记录写入、按序返回每次读取结果的可控接缝。 */
function scriptedRpc(reads: unknown[], onWrite?: (params: unknown) => void): PluginRpc {
  return (async (method: string, params: unknown): Promise<unknown> => {
    if (method === "plugin/settings/read") {
      return reads.length > 1 ? reads.shift() : reads[0];
    }
    if (method === "plugin/settings/write") {
      onWrite?.(params);
      return { ok: true };
    }
    return undefined;
  }) as unknown as PluginRpc;
}

describe("SuperpowersKanbanPluginSettings", () => {
  it("reads only on mount and never resets unsaved edits when the rpc identity changes", async () => {
    let reads = 0;
    const { rerender } = render(<SuperpowersKanbanPluginSettings rpc={makeRpc(() => reads++)} />);

    const input = (await screen.findByLabelText("默认并发上限")) as HTMLInputElement;
    fireEvent.change(input, { target: { value: "7" } });
    expect(input.value).toBe("7");

    // 宿主重渲染换新 rpc 身份（App 的 2 秒看板轮询会驱动它）。
    rerender(<SuperpowersKanbanPluginSettings rpc={makeRpc(() => reads++)} />);
    await act(async () => {
      await Promise.resolve();
    });

    // 挂载时读过一次；身份变化不得再读。
    expect(reads).toBe(1);
    // 用户没保存的输入必须还在。
    expect((screen.getByLabelText("默认并发上限") as HTMLInputElement).value).toBe("7");
  });

  it("re-reads after a successful save and renders the persisted values", async () => {
    // 写是全量替换、插件会规范化，所以保存后以前端重读的结果为准（规范 §7.3）。
    const reads = [
      { settings: EMPTY_SETTINGS },
      {
        settings: {
          default_max_tasks: 3,
          interval_secs: 10,
          windows: [{ days: "Mon", start: "09:00", end: "17:00", all_day: false, max_tasks: 5 }],
        },
      },
    ];
    let writes = 0;
    const rpc = scriptedRpc(reads, () => writes++);

    render(<SuperpowersKanbanPluginSettings rpc={rpc} />);
    await screen.findByLabelText("默认并发上限");
    fireEvent.click(screen.getByRole("button", { name: "保存" }));

    // 重读回来的窗口必须渲染出来。
    await screen.findByLabelText("窗口 0 星期");
    expect((screen.getByLabelText("窗口 0 上限") as HTMLInputElement).value).toBe("5");
    expect(writes).toBe(1);
  });

  it("surfaces the plugin's rejection message instead of 插件未运行", async () => {
    const rpc = (async (method: string): Promise<unknown> => {
      if (method === "plugin/settings/read") return { settings: EMPTY_SETTINGS };
      if (method === "plugin/settings/write") {
        throw {
          code: -32024,
          message:
            "the plugin rejected the query: Validation interval_secs must be in [1, 3600], got 0",
          data: { code: "plugin_rejected" },
        };
      }
      return undefined;
    }) as unknown as PluginRpc;

    render(<SuperpowersKanbanPluginSettings rpc={rpc} />);
    const input = (await screen.findByLabelText("推进间隔秒数")) as HTMLInputElement;
    fireEvent.change(input, { target: { value: "20" } });
    fireEvent.click(screen.getByRole("button", { name: "保存" }));

    const alert = await screen.findByRole("alert");
    expect(alert.textContent ?? "").toContain("interval_secs");
    expect(alert.textContent ?? "").not.toContain("插件未运行");
    // 没有乐观更新：用户输入仍在。
    expect((screen.getByLabelText("推进间隔秒数") as HTMLInputElement).value).toBe("20");
  });

  it("keeps the blanket copy only for a genuinely unavailable plugin", async () => {
    const rpc = (async (method: string): Promise<unknown> => {
      if (method === "plugin/settings/read") return { settings: EMPTY_SETTINGS };
      if (method === "plugin/settings/write") {
        throw { code: -32022, message: "plugin superpowers-kanban is not available", data: { code: "plugin_unavailable" } };
      }
      return undefined;
    }) as unknown as PluginRpc;

    render(<SuperpowersKanbanPluginSettings rpc={rpc} />);
    await screen.findByLabelText("默认并发上限");
    fireEvent.click(screen.getByRole("button", { name: "保存" }));

    expect((await screen.findByRole("alert")).textContent).toBe("插件未运行，保存失败");
  });

  it("does not turn an emptied numeric input into 0 or NaN", async () => {
    render(<SuperpowersKanbanPluginSettings rpc={scriptedRpc([{ settings: EMPTY_SETTINGS }])} />);
    const maxTasks = (await screen.findByLabelText("默认并发上限")) as HTMLInputElement;
    const interval = screen.getByLabelText("推进间隔秒数") as HTMLInputElement;

    fireEvent.change(maxTasks, { target: { value: "" } });
    fireEvent.change(interval, { target: { value: "" } });

    // 清空被忽略，保留上一个值（不是 0、不是 NaN）。
    expect(maxTasks.value).toBe("3");
    expect(interval.value).toBe("10");
    await waitFor(() => expect(screen.queryByRole("alert")).toBeNull());
  });
});
