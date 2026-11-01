/** @vitest-environment jsdom */
import { afterEach, describe, it, expect } from "vitest";
import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { RemoteSettings, type RemoteInvoke } from "./RemoteSettings";

/** 最小的 IPC 替身：按 command 名回答，其余命令记录参数。 */
function fakeInvoke(answers: Record<string, unknown>): {
  invoke: RemoteInvoke;
  calls: Array<{ command: string; args?: Record<string, unknown> }>;
} {
  const calls: Array<{ command: string; args?: Record<string, unknown> }> = [];
  const invoke: RemoteInvoke = async <T,>(
    command: string,
    args?: Record<string, unknown>,
  ) => {
    calls.push({ command, args });
    if (command in answers) return answers[command] as T;
    return null as T;
  };
  return { invoke, calls };
}

afterEach(cleanup);

describe("RemoteSettings", () => {
  /**
   * dev-only 功能在 stable 上不该有任何痕迹：闸门关着连标题都不该出现。
   */
  it("renders nothing when the gate is off", async () => {
    const { invoke } = fakeInvoke({ remote_enabled: false });
    const { container } = render(<RemoteSettings invoke={invoke} />);
    await waitFor(() => expect(screen.queryByText("远程项目")).toBeNull());
    expect(container.innerHTML).toBe("");
  });

  /**
   * 远端连续失败 5 次后停止自动重试。此时只说「已断开」的话，用户唯一能做的
   * 只有无意义地等待，所以必须给出原因与手动重连入口。
   */
  it("shows the disconnect reason and a manual reconnect button", async () => {
    const { invoke } = fakeInvoke({
      remote_enabled: true,
      remote_list: {
        enabled: true,
        profiles: [
          {
            id: "proj-foo",
            label: "foo",
            host: "dev@host",
            local_path: "/Users/y/foo",
            remote_path: "/home/dev/foo",
            remote_arch: "x86_64-unknown-linux-gnu",
            binary_version: "0.1.10",
          },
        ],
      },
      remote_status: {
        slots: {
          "proj-foo": {
            status: "disconnected",
            lastError: "gave up after 5 attempts",
          },
        },
      },
    });
    render(<RemoteSettings invoke={invoke} />);
    expect(await screen.findByText(/gave up after 5 attempts/)).toBeTruthy();
    expect(screen.getByRole("button", { name: "重连" })).toBeTruthy();
  });

  it("lists a configured project with its host and remote path", async () => {
    const { invoke } = fakeInvoke({
      remote_enabled: true,
      remote_list: {
        enabled: true,
        profiles: [
          {
            id: "proj-foo",
            label: "foo",
            host: "dev@host",
            local_path: "/Users/y/foo",
            remote_path: "/home/dev/foo",
            remote_arch: "x86_64-unknown-linux-gnu",
            binary_version: "0.1.10",
          },
        ],
      },
      remote_status: { slots: {} },
    });
    render(<RemoteSettings invoke={invoke} />);
    expect(await screen.findByText(/dev@host/)).toBeTruthy();
    // 远端路径同时出现在行内摘要与（打开表单后）输入框里，故只断言存在。
    expect(screen.getAllByText(/\/home\/dev\/foo/).length).toBeGreaterThan(0);
  });

  /** 未在 `remote_status` 里出现过的 profile 按断开处理，不是「未知」。 */
  it("treats a profile with no slot state as disconnected", async () => {
    const { invoke } = fakeInvoke({
      remote_enabled: true,
      remote_list: {
        enabled: true,
        profiles: [
          {
            id: "proj-foo",
            label: "foo",
            host: "dev@host",
            local_path: "/Users/y/foo",
            remote_path: "/home/dev/foo",
            remote_arch: "x86_64-unknown-linux-gnu",
            binary_version: "0.1.10",
          },
        ],
      },
      remote_status: { slots: {} },
    });
    render(<RemoteSettings invoke={invoke} />);
    expect(await screen.findByText("已断开")).toBeTruthy();
    expect(screen.getByRole("button", { name: "重连" })).toBeTruthy();
  });

  it("connects through remote_connect with the profile id", async () => {
    const { invoke, calls } = fakeInvoke({
      remote_enabled: true,
      remote_list: {
        enabled: true,
        profiles: [
          {
            id: "proj-foo",
            label: "foo",
            host: "dev@host",
            local_path: "/Users/y/foo",
            remote_path: "/home/dev/foo",
            remote_arch: "x86_64-unknown-linux-gnu",
            binary_version: "0.1.10",
          },
        ],
      },
      remote_status: { slots: {} },
    });
    render(<RemoteSettings invoke={invoke} />);
    fireEvent.click(await screen.findByRole("button", { name: "重连" }));
    await waitFor(() =>
      expect(calls.some((call) => call.command === "remote_connect")).toBe(
        true,
      ),
    );
    // `from` 常态为 null：路径 3 是兜底，不该在未失败时带上任何东西。
    expect(
      calls.find((call) => call.command === "remote_connect")?.args,
    ).toEqual({
      profileId: "proj-foo",
      from: null,
    });
  });

  /**
   * 路径 3 的入口只在失败后出现。能自动装成时给一个「改用本地二进制」按钮，
   * 会让人以为必须自己准备二进制——这类提示会把常规路径变成看起来像例外。
   */
  it("only offers the local-binary fallback after a failure", async () => {
    const { invoke } = fakeInvoke({
      remote_enabled: true,
      remote_list: {
        enabled: true,
        profiles: [
          {
            id: "proj-foo",
            label: "foo",
            host: "dev@host",
            local_path: "/Users/y/foo",
            remote_path: "/home/dev/foo",
            remote_arch: "x86_64-unknown-linux-gnu",
            binary_version: "0.1.10",
          },
        ],
      },
      remote_status: { slots: { "proj-foo": { status: "connected", lastError: null } } },
    });
    render(<RemoteSettings invoke={invoke} />);
    await screen.findByText(/dev@host/);
    expect(screen.queryByRole("button", { name: "改用本地二进制" })).toBeNull();
  });

  it("offers the local-binary fallback once there is an error to explain", async () => {
    const { invoke } = fakeInvoke({
      remote_enabled: true,
      remote_list: {
        enabled: true,
        profiles: [
          {
            id: "proj-foo",
            label: "foo",
            host: "dev@host",
            local_path: "/Users/y/foo",
            remote_path: "/home/dev/foo",
            remote_arch: "x86_64-unknown-linux-gnu",
            binary_version: "0.1.10",
          },
        ],
      },
      remote_status: {
        slots: { "proj-foo": { status: "disconnected", lastError: "下载失败" } },
      },
    });
    render(<RemoteSettings invoke={invoke} />);
    expect(
      await screen.findByRole("button", { name: "改用本地二进制" }),
    ).toBeTruthy();
  });

  /**
   * `instance_root` 按 profile id 派生（spec §4.9），表单里**不能**有它的输入框：
   * 让用户填会让两个项目指向同一目录，配置看起来对但互相覆盖。
   */
  it("does not offer instance_root as a field", async () => {
    const { invoke } = fakeInvoke({
      remote_enabled: true,
      remote_list: { enabled: true, profiles: [] },
      remote_status: { slots: {} },
    });
    render(<RemoteSettings invoke={invoke} />);
    fireEvent.click(await screen.findByRole("button", { name: "新增" }));
    expect(screen.queryByText(/instance_root/)).toBeNull();
    // 同一个表单里该有的字段仍在，证明上面那条不是因为表单没渲染才通过。
    expect(screen.getByText(/远端目录（那台机器上的项目路径）/)).toBeTruthy();
  });

  it("surfaces an upsert error instead of failing silently", async () => {
    const invoke: RemoteInvoke = async <T,>(command: string) => {
      if (command === "remote_enabled") return true as T;
      if (command === "remote_list")
        return { enabled: true, profiles: [] } as T;
      if (command === "remote_status") return { slots: {} } as T;
      if (command === "remote_upsert") throw new Error("bad profile id");
      return null as T;
    };
    render(<RemoteSettings invoke={invoke} />);
    fireEvent.click(await screen.findByRole("button", { name: "新增" }));
    fireEvent.click(screen.getByRole("button", { name: "保存" }));
    expect(await screen.findByRole("alert")).toHaveProperty(
      "textContent",
      expect.stringContaining("bad profile id"),
    );
  });
});
