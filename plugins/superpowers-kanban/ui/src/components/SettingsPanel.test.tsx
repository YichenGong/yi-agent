import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { PluginHostApi } from "@yi-agent/plugin-sdk";
import { METHODS } from "../lib/methods";
import { SettingsPanel } from "./SettingsPanel";

interface Settings {
  default_max_tasks: number;
  interval_secs: number;
  windows: Array<{
    days: string;
    start: string;
    end: string;
    all_day: boolean;
    max_tasks: number;
  }>;
}

const SETTINGS: Settings = {
  default_max_tasks: 3,
  interval_secs: 30,
  windows: [{ days: "Mon-Fri", start: "09:00", end: "24:00", all_day: false, max_tasks: 2 }],
};

/** A host stub that answers only the two methods this panel declares. */
function host(overrides: {
  read?: () => Promise<unknown>;
  write?: (params: unknown) => Promise<unknown>;
} = {}): PluginHostApi {
  return {
    context: async () => ({
      api: "v1",
      plugin: { id: "superpowers-kanban", version: "0.1.0" },
      host: { id: "h1", version: "0.1.0" },
      project: { handle: "ph1_test" },
      activation: 1,
      theme: "dark",
    }),
    request: vi.fn(async (method: string, params: unknown) => {
      if (method === METHODS.settingsRead) {
        return overrides.read ? await overrides.read() : { settings: SETTINGS };
      }
      if (method === METHODS.settingsWrite) {
        return overrides.write ? await overrides.write(params) : {};
      }
      throw new Error(`unexpected method ${method}`);
    }),
    result: async () => ({ status: "committed", requestId: "r", result: null }),
    subscribe: async () => () => {},
    openThread: async () => {},
    notify: async () => {},
    confirm: async () => false,
  } as unknown as PluginHostApi;
}

describe("SettingsPanel", () => {
  it("reads the settings once and renders them", async () => {
    const h = host();
    render(<SettingsPanel host={h} />);
    expect(await screen.findByLabelText("全机并发上限")).toHaveValue(3);
    expect(screen.getByLabelText("推进间隔秒数")).toHaveValue(30);
    expect(screen.getByLabelText("窗口 0 上限")).toHaveValue(2);
  });

  it("does not re-read when the host identity changes, keeping unsaved edits", async () => {
    const reads = vi.fn(async () => ({ settings: SETTINGS }));
    const { rerender } = render(<SettingsPanel host={host({ read: reads })} />);
    const cap = await screen.findByLabelText("全机并发上限");
    // One change event with the whole new value: the field is controlled and
    // ignores an empty string, so typing character by character would leave the
    // caret fighting the re-render.
    fireEvent.change(cap, { target: { value: "7" } });
    expect(screen.getByLabelText("全机并发上限")).toHaveValue(7);
    // The desktop remounts views often, and every remount can carry a new host
    // object. Reloading on identity would silently discard what the user typed.
    rerender(<SettingsPanel host={host({ read: reads })} />);
    await waitFor(() => expect(reads).toHaveBeenCalledTimes(1));
    expect(screen.getByLabelText("全机并发上限")).toHaveValue(7);
  });

  it("writes the whole document and re-reads the normalized result", async () => {
    // Typed parameter: the assertion below inspects the params it was called
    // with, and an untyped mock records a zero-length argument tuple.
    const write = vi.fn(async (_params: unknown) => ({}));
    const read = vi
      .fn<() => Promise<unknown>>()
      .mockResolvedValueOnce({ settings: SETTINGS })
      .mockResolvedValueOnce({ settings: { ...SETTINGS, default_max_tasks: 5 } });
    const user = userEvent.setup();
    render(<SettingsPanel host={host({ read, write })} />);
    const cap = await screen.findByLabelText("全机并发上限");
    fireEvent.change(cap, { target: { value: "5" } });
    await user.click(screen.getByRole("button", { name: "保存" }));

    await waitFor(() => expect(write).toHaveBeenCalledTimes(1));
    expect(write.mock.calls[0][0]).toMatchObject({
      settings: expect.objectContaining({ default_max_tasks: 5 }),
    });
    // The backend is the normalizing authority, so the displayed value comes
    // from the re-read, not from what was typed.
    await waitFor(() => expect(read).toHaveBeenCalledTimes(2));
    expect(screen.getByLabelText("全机并发上限")).toHaveValue(5);
  });

  it("surfaces the backend's own rejection instead of a blanket message", async () => {
    const user = userEvent.setup();
    render(
      <SettingsPanel
        host={host({
          write: async () => {
            throw { kind: "Conflict", detail: "interval_secs 超出范围" };
          },
        })}
      />,
    );
    await screen.findByLabelText("全机并发上限");
    await user.click(screen.getByRole("button", { name: "保存" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("interval_secs 超出范围");
  });

  it("keeps the blanket copy only for a genuinely unavailable backend", async () => {
    const user = userEvent.setup();
    render(
      <SettingsPanel
        host={host({
          write: async () => {
            throw { kind: "NotSupported", detail: "plugin_unavailable: not running" };
          },
        })}
      />,
    );
    await screen.findByLabelText("全机并发上限");
    await user.click(screen.getByRole("button", { name: "保存" }));
    expect(await screen.findByRole("alert")).toHaveTextContent(/看板后端未运行/);
  });

  it("reports an unknown write outcome instead of claiming failure", async () => {
    const user = userEvent.setup();
    // A backend whose ledger cannot say: the write's reply was lost and looking
    // the id up answers "still unknown". Claiming failure would invite a retry
    // that could duplicate the write.
    const h = host({
      write: async () => {
        throw { kind: "ResultUnknown", detail: "no answer" };
      },
    });
    const base = h as unknown as {
      result: (id: string) => Promise<unknown>;
      request: (method: string, params: unknown) => Promise<unknown>;
    };
    base.result = async (id: string) => ({ status: "unknown", requestId: id, result: null });
    const original = base.request;
    base.request = async (method: string, params: unknown) => {
      if (method === METHODS.operation) {
        return { status: "unknown", request_id: (params as { request_id?: string }).request_id };
      }
      return original(method, params);
    };
    render(<SettingsPanel host={h} />);
    await screen.findByLabelText("全机并发上限");
    await user.click(screen.getByRole("button", { name: "保存" }));
    expect(await screen.findByRole("alert")).toHaveTextContent(/结果未知/);
  });

  it("does not turn an emptied numeric input into 0 or NaN", async () => {
    const user = userEvent.setup();
    render(<SettingsPanel host={host()} />);
    const cap = await screen.findByLabelText("全机并发上限");
    const interval = screen.getByLabelText("推进间隔秒数");

    // Clearing is ignored and the previous value stays: 0 would ask the backend
    // for a budget it cannot honor, and NaN would fail schema validation.
    fireEvent.change(cap, { target: { value: "" } });
    fireEvent.change(interval, { target: { value: "" } });
    expect(screen.getByLabelText("全机并发上限")).toHaveValue(3);
    expect(screen.getByLabelText("推进间隔秒数")).toHaveValue(30);

    // Junk is ignored for the same reason.
    fireEvent.change(cap, { target: { value: "abc" } });
    expect(screen.getByLabelText("全机并发上限")).toHaveValue(3);

    await user.click(screen.getByRole("button", { name: "保存" }));
    await waitFor(() => expect(screen.getByLabelText("全机并发上限")).toHaveValue(3));
  });

  it("toggling a day chip writes the compressed spec back", async () => {
    const user = userEvent.setup();
    render(<SettingsPanel host={host()} />);
    await screen.findByLabelText("窗口 0 星期一");
    await user.click(screen.getByLabelText("窗口 0 星期一"));
    // Mon-Fri minus Monday is Tue-Fri, not "Tue,Wed,Thu,Fri".
    expect(screen.getByLabelText("窗口 0 星期四")).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByLabelText("窗口 0 星期一")).toHaveAttribute("aria-pressed", "false");
  });

  it("keeps the last selected day, since the backend rejects an empty spec", async () => {
    const read = async () => ({
      settings: {
        ...SETTINGS,
        windows: [{ days: "Wed", start: "09:00", end: "17:00", all_day: false, max_tasks: 1 }],
      },
    });
    render(<SettingsPanel host={host({ read })} />);
    const wed = await screen.findByLabelText("窗口 0 星期三");
    expect(wed).toBeDisabled();
    expect(wed).toHaveAttribute("title", "至少保留一天");
  });

  it("removes a window when its delete button is clicked", async () => {
    const user = userEvent.setup();
    render(<SettingsPanel host={host()} />);
    await screen.findByLabelText("窗口 0 上限");
    await user.click(screen.getByRole("button", { name: "删除窗口 0" }));
    expect(screen.queryByLabelText("窗口 0 上限")).toBeNull();
    expect(screen.getByText("未设置时段窗口")).toBeTruthy();
  });

  it("adds a new window with sensible defaults", async () => {
    const user = userEvent.setup();
    render(<SettingsPanel host={host()} />);
    await screen.findByLabelText("窗口 0 上限");
    await user.click(screen.getByRole("button", { name: "添加窗口" }));
    expect(screen.getByLabelText("窗口 1 上限")).toHaveValue(3);
    expect(screen.getByLabelText("窗口 1 开始")).toHaveValue("09:00");
  });

  it("labels the concurrency cap as machine-wide, not board-wide", async () => {
    render(<SettingsPanel host={host()} />);
    await screen.findByLabelText("全机并发上限");
    expect(screen.getByText(/共用这个上限/)).toBeTruthy();
  });

  it("states which clock the window hours follow", async () => {
    render(<SettingsPanel host={host()} />);
    await screen.findByLabelText("全机并发上限");
    expect(screen.getByTestId("window-timezone-note")).toHaveTextContent(/运行插件的机器/);
  });

  it("explains that settings cannot be read without a host", () => {
    render(<SettingsPanel host={null} />);
    expect(screen.getByRole("alert")).toHaveTextContent(/未连接到宿主/);
  });
});
