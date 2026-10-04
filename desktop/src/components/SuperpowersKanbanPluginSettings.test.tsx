/** @vitest-environment jsdom */
import { afterEach, describe, expect, it } from "vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import type { PluginRpc } from "../lib/pluginSettings";
import { SuperpowersKanbanPluginSettings } from "./SuperpowersKanbanPluginSettings";

afterEach(cleanup);

/** 每次调用都返回一个新的函数身份，模拟宿主每次渲染换新接缝。 */
function makeRpc(onRead: () => void): PluginRpc {
  return async <T,>(method: string): Promise<T> => {
    if (method === "plugin/settings/read") {
      onRead();
      return { settings: { default_max_tasks: 3, interval_secs: 10, windows: [] } } as T;
    }
    return undefined as T;
  };
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
});
