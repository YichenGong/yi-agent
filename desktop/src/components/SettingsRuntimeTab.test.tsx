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

  it("shows enum options in Chinese but submits the raw value", async () => {
    const sandbox = field({
      key: "startup.sandbox",
      group: "startup",
      label: "默认沙箱档位",
      type: "select",
      options: ["read-only", "workspace-write", "danger-full-access"],
      default: "workspace-write",
      description: "启动时的文件写入沙箱；空表示用默认档位",
      min: null,
    });
    const call = callWith([sandbox]);
    render(<SettingsRuntimeTab call={call} />);

    // 默认值本身也要中文化，否则下拉里是中文、旁边写着英文。
    expect(await screen.findByText("默认 工作区可写")).toBeTruthy();

    const select = (await screen.findByLabelText("默认沙箱档位")) as HTMLSelectElement;
    const labels = Array.from(select.options).map((o) => o.textContent);
    expect(labels).toContain("只读");
    expect(labels).toContain("完全访问");
    // 未设置项显示默认值的中文。
    expect(labels[0]).toBe("未设置（默认 工作区可写）");

    // 选中文项，提交的仍是原始字符串。
    fireEvent.change(select, { target: { value: "danger-full-access" } });
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    await waitFor(() => {
      expect(call).toHaveBeenCalledWith("settings/write", {
        patch: { "startup.sandbox": "danger-full-access" },
      });
    });
  });

  it("uses a multi-line textarea for the free-text system prompt", async () => {
    const prompt = field({
      key: "agent.system_prompt",
      label: "系统提示词",
      type: "text",
      default: "",
      description: "覆盖默认系统提示词；留空表示用默认",
      min: null,
    });
    render(<SettingsRuntimeTab call={callWith([prompt])} />);
    const box = (await screen.findByLabelText("系统提示词")) as HTMLElement;
    expect(box.tagName).toBe("TEXTAREA");
    // 空默认值显示成破折号，不是空白。
    expect(screen.getByText("默认 —")).toBeTruthy();
  });

describe("runtime settings labels", () => {
  it("names every backend group in Chinese", () => {
    // 与 yi-agent-runtime/src/settings.rs 的 FIELDS 分组一一对应。漏一个就会在页面上
    // 露出英文组名（tui 曾经就是这样）。
    //
    // 不写精确 toEqual：这张表会随功能增长，每加一组就要改一次期望数组，
    // 而本测试真正要守的是「没有组漏出英文名」。下面两条循环断言已覆盖它，
    // toContain 再钉住顺序表里确实登记了新组。
    for (const group of GROUP_ORDER) {
      expect(GROUP_LABELS[group]).toBeTruthy();
      expect(GROUP_HINTS[group]).toBeTruthy();
    }
    expect(GROUP_ORDER).toContain("power");
  });

  it("translates enum values but keeps unknown ones verbatim", () => {
    expect(labelFor("startup.sandbox", "workspace-write")).toBe("工作区可写");
    expect(labelFor("tui.notification_method", "bel")).toBe("响铃");
    expect(labelFor("tui.terminal_title", "false")).toBe("关");
    expect(labelFor("tui.terminal_title", "true")).toBe("开");
    // 后端将来加选项时，前端不该留空或抛错。
    expect(labelFor("startup.sandbox", "some-future-mode")).toBe("some-future-mode");
    // 没有映射表的字段（数字）原样返回。
    expect(labelFor("agent.max_turns", "200")).toBe("200");
    // `default_display` 可能是空串（agent.system_prompt），显示成破折号。
    expect(labelFor("agent.system_prompt", "")).toBe("—");
  });

  it("translates the power switch like the other on/off selects", () => {
    // 漏加映射不会报错，只会让下拉框显示英文 true/false，与相邻的
    // 「终端通知」组风格不一致。
    expect(labelFor("power.prevent_idle_sleep", "true")).toBe("开");
    expect(labelFor("power.prevent_idle_sleep", "false")).toBe("关");
  });
});

/**
 * 按平台收窄：手机端不显示「终端通知」组。
 *
 * 这一组的键（`tui.notifications` / `tui.notification_method` /
 * `tui.terminal_title` / `tui.notification_condition`）整的是**桌面主机的命令行
 * 界面**：配对手机改它，改的是另一台机器上 TUI 的行为，而手机自己永远看不到
 * 那些提示。这正是「页面只显示它需要的参数」。
 */
describe("SettingsRuntimeTab platform scoping", () => {
  const tuiFields = [
    field({ key: "agent.max_turns" }),
    field({
      key: "tui.notifications",
      group: "tui",
      label: "通知方式",
      type: "select",
      options: ["true", "false"],
    }),
  ];

  it("hides the desktop-only tui group on the phone", async () => {
    render(<SettingsRuntimeTab call={callWith(tuiFields)} isMobile />);
    expect(await screen.findByText("最大轮次")).toBeTruthy();
    // 组名与字段名都叫「终端通知」，故用组标题的 role 定位，避免歧义。
    expect(screen.queryByRole("heading", { name: "终端通知" })).toBeNull();
    expect(screen.queryByText("通知方式")).toBeNull();
  });

  it("keeps the tui group on the desktop", async () => {
    render(<SettingsRuntimeTab call={callWith(tuiFields)} />);
    expect(
      await screen.findByRole("heading", { name: "终端通知" }),
    ).toBeTruthy();
  });

  it("still round-trips every field in the draft, including hidden ones", async () => {
    // 隐藏只是不渲染：`buildPatch` 与草稿仍按**全量**字段建，否则手机端保存
    // 一次就会把桌面端的 tui 值当成「未改动」而从 patch 里漏掉——那不会写坏
    // 数据（patch 只含有改动项），但会让下方「改了 N 项」的计数与撤销失真。
    const call = callWith(tuiFields);
    render(<SettingsRuntimeTab call={call} isMobile />);
    // 输入框的值来自草稿（未设置时为空串），故按 `<label for>` 定位。
    const input = (await screen.findByLabelText("最大轮次")) as HTMLInputElement;
    fireEvent.change(input, { target: { value: "100" } });
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    await waitFor(() =>
      expect(call).toHaveBeenCalledWith("settings/write", {
        patch: { "agent.max_turns": "100" },
      }),
    );
  });
});

describe("SettingsRuntimeTab concurrency", () => {
  const deferred = <T,>() => {
    let resolve!: (value: T) => void;
    const promise = new Promise<T>((r) => {
      resolve = r;
    });
    return { promise, resolve };
  };

  it("keeps edits made while a save is pending", async () => {
    // The user submits 201, then types 202 before the host answers. The save
    // landing must not clear the 202 they are looking at.
    const pendingSave = deferred<unknown>();
    const call = vi.fn(async (method: string) => {
      if (method === "settings/read") {
        return { live_apply_version: 1, fields: [field({ value: 200 })], path: "/x/settings.json" };
      }
      return pendingSave.promise;
    });
    render(<SettingsRuntimeTab call={call} />);
    const input = (await screen.findByLabelText("最大轮次")) as HTMLInputElement;
    fireEvent.change(input, { target: { value: "201" } });
    fireEvent.click(screen.getByRole("button", { name: /^保存$/ }));
    fireEvent.change(input, { target: { value: "202" } });

    // The host now reports it stored 201.
    call.mockImplementation(async (method: string) => {
      if (method === "settings/read") {
        return { live_apply_version: 1, fields: [field({ value: 201 })], path: "/x/settings.json" };
      }
      return { ok: true, live_apply_version: 1, changed_keys: ["agent.max_turns"] };
    });
    pendingSave.resolve({ ok: true, live_apply_version: 1, changed_keys: ["agent.max_turns"] });

    // Wait for the save to settle first: asserting straight after resolving would
    // pass while the value is still 202 from the user's keystroke, then fail to
    // notice the draft being discarded a moment later. A mutation that ignores
    // edit generations must turn this red, so the read has to run first.
    await waitFor(() => {
      expect(screen.getByRole("button", { name: /^保存$/ })).toBeTruthy();
      expect((screen.getByRole("button", { name: /^保存$/ }) as HTMLButtonElement).disabled).toBe(
        false,
      );
    });
    await waitFor(() => {
      expect((screen.getByLabelText("最大轮次") as HTMLInputElement).value).toBe("202");
    });
  });

  it("adopts a remote change for a field the user has not touched", async () => {
    let invalidate: (() => void) | null = null;
    const subscribe = (listener: () => void) => {
      invalidate = listener;
      return () => {
        invalidate = null;
      };
    };
    let value = 200;
    const call = vi.fn(async (method: string) => {
      if (method === "settings/read") {
        return { live_apply_version: 1, fields: [field({ value })], path: "/x/settings.json" };
      }
      return { ok: true, live_apply_version: 1 };
    });
    render(<SettingsRuntimeTab call={call} subscribe={subscribe} />);
    expect(await screen.findByText("已设为 200")).toBeTruthy();

    // Another window saved 250; the host says "settings changed".
    value = 250;
    fireEvent.click(screen.getByRole("button", { name: /保存/ })); // no-op: nothing dirty
    expect(invalidate).toBeTypeOf("function");
    invalidate!();

    await waitFor(() => expect(screen.getByText("已设为 250")).toBeTruthy());
  });

  it("keeps the user's draft when the remote changed the same field", async () => {
    let invalidate: (() => void) | null = null;
    const subscribe = (listener: () => void) => {
      invalidate = listener;
      return () => {
        invalidate = null;
      };
    };
    let value = 200;
    const call = vi.fn(async (method: string) => {
      if (method === "settings/read") {
        return { live_apply_version: 1, fields: [field({ value })], path: "/x/settings.json" };
      }
      return { ok: true, live_apply_version: 1 };
    });
    render(<SettingsRuntimeTab call={call} subscribe={subscribe} />);
    const input = (await screen.findByLabelText("最大轮次")) as HTMLInputElement;
    fireEvent.change(input, { target: { value: "300" } });

    value = 250;
    invalidate!();

    await waitFor(() => expect(screen.getByText(/这些项在别处也被改过/)).toBeTruthy());
    expect((screen.getByLabelText("最大轮次") as HTMLInputElement).value).toBe("300");
  });

  it("ignores a late read from an abandoned request", async () => {
    // Two reads race: the first is slow. Its response must not overwrite what the
    // second (authoritative) read already showed.
    const first = deferred<unknown>();
    let readCount = 0;
    let invalidate: (() => void) | null = null;
    const subscribe = (listener: () => void) => {
      invalidate = listener;
      return () => {
        invalidate = null;
      };
    };
    const call = vi.fn(async (method: string) => {
      if (method !== "settings/read") return { ok: true, live_apply_version: 1 };
      readCount += 1;
      if (readCount === 1) return first.promise;
      return { live_apply_version: 1, fields: [field({ value: 900 })], path: "/x/settings.json" };
    });
    render(<SettingsRuntimeTab call={call} subscribe={subscribe} />);
    // The first read is still pending; a notification triggers the second.
    invalidate!();
    await waitFor(() => expect(screen.getByText("已设为 900")).toBeTruthy());

    // Now the stale first read lands with an older value.
    first.resolve({ live_apply_version: 1, fields: [field({ value: 100 })], path: "/x/settings.json" });
    await new Promise((r) => setTimeout(r, 20));
    expect(screen.getByText("已设为 900")).toBeTruthy();
    expect(screen.queryByText("已设为 100")).toBeNull();
  });

  it("says only 已保存 when the host does not report live apply", async () => {
    // An old host answers `{ok:true}` with no capability. Promising a hot reload
    // there would be a lie the user could not detect until much later.
    const call = vi.fn(async (method: string) => {
      if (method === "settings/read") return { fields: [field({ value: 200 })], path: "/x/settings.json" };
      return { ok: true };
    });
    render(<SettingsRuntimeTab call={call} />);
    const input = (await screen.findByLabelText("最大轮次")) as HTMLInputElement;
    fireEvent.change(input, { target: { value: "210" } });
    fireEvent.click(screen.getByRole("button", { name: /^保存$/ }));

    await waitFor(() => expect(screen.getByText("已保存。")).toBeTruthy());
    expect(screen.queryByText(/下一个安全执行点/)).toBeNull();
  });

  it("promises live apply when the host reports it", async () => {
    const call = vi.fn(async (method: string) => {
      if (method === "settings/read") {
        return { live_apply_version: 1, fields: [field({ value: 200 })], path: "/x/settings.json" };
      }
      return { ok: true, live_apply_version: 1, changed_keys: ["agent.max_turns"] };
    });
    render(<SettingsRuntimeTab call={call} />);
    const input = (await screen.findByLabelText("最大轮次")) as HTMLInputElement;
    fireEvent.change(input, { target: { value: "210" } });
    fireEvent.click(screen.getByRole("button", { name: /^保存$/ }));

    await waitFor(() => expect(screen.getByText(/下一个安全执行点/)).toBeTruthy());
  });

  it("adds the sandbox exception when the batch touches the default sandbox", async () => {
    const sandbox = field({
      key: "startup.sandbox",
      group: "startup",
      label: "默认沙箱",
      type: "select",
      value: "read-only",
      options: ["read-only", "workspace-write"],
    });
    const call = vi.fn(async (method: string) => {
      if (method === "settings/read") {
        return { live_apply_version: 1, fields: [sandbox], path: "/x/settings.json" };
      }
      return { ok: true, live_apply_version: 1, changed_keys: ["startup.sandbox"] };
    });
    render(<SettingsRuntimeTab call={call} />);
    const select = (await screen.findByLabelText("默认沙箱")) as HTMLSelectElement;
    fireEvent.change(select, { target: { value: "workspace-write" } });
    fireEvent.click(screen.getByRole("button", { name: /^保存$/ }));

    await waitFor(() => expect(screen.getByText(/仅影响保存后新建会话/)).toBeTruthy());
  });

  it("keeps the draft and shows the error when a read fails", async () => {
    // A failed refresh must not wipe what the user typed, and must not leave the
    // last success message standing as if nothing happened.
    let fail = false;
    const call = vi.fn(async (method: string) => {
      if (method === "settings/read") {
        if (fail) throw new Error("read failed");
        return { live_apply_version: 1, fields: [field({ value: 200 })], path: "/x/settings.json" };
      }
      return { ok: true, live_apply_version: 1 };
    });
    let invalidate: (() => void) | null = null;
    const subscribe = (listener: () => void) => {
      invalidate = listener;
      return () => {
        invalidate = null;
      };
    };
    render(<SettingsRuntimeTab call={call} subscribe={subscribe} />);
    const input = (await screen.findByLabelText("最大轮次")) as HTMLInputElement;
    fireEvent.change(input, { target: { value: "777" } });

    fail = true;
    invalidate!();
    await waitFor(() => expect(screen.getByText(/read failed/)).toBeTruthy());
    expect((screen.getByLabelText("最大轮次") as HTMLInputElement).value).toBe("777");
  });
});
