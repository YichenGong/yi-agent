/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, renderHook } from "@testing-library/react";
import { useSettingsState } from "./useSettingsState";
import { FONT_SCALE_STORAGE_KEY, type FontScale } from "../lib/fontScale";

afterEach(() => {
  localStorage.clear();
  delete document.documentElement.dataset.fontScale;
  delete document.documentElement.dataset.theme;
});

/** 一个只回成功的 RPC 替身：记录每次 `ui/settings/write` 的参数。 */
function okRpc() {
  const calls: Array<{ method: string; params: unknown }> = [];
  const client = {
    request: async (method: string, params: unknown) => {
      calls.push({ method, params });
      return { ok: true };
    },
  };
  // 接缝的形状只要求 `request`，故这里不必造完整的 RpcClient。
  return { calls, rpc: () => client as never };
}

const mount = (opts: { isMobile?: boolean; rpc?: ReturnType<typeof okRpc> } = {}) =>
  renderHook(() =>
    useSettingsState({
      rpc: (opts.rpc ?? okRpc()).rpc,
      force: vi.fn(),
      setCurrentError: vi.fn(),
      isMobile: opts.isMobile ?? false,
    }),
  );

describe("useSettingsState font scale", () => {
  it("starts the phone on the small tier and the desktop on standard", () => {
    // 用户报的是「手机上字有点大」。首启档位必须按平台分开，否则要么手机
    // 没变小，要么桌面观感被改动。
    expect(mount({ isMobile: true }).result.current.fontScale).toBe("small");
    // 首屏缓存是**每台设备自己的** localStorage；测试里两次挂载共用一份，
    // 故先清掉，模拟「这是另一台从没配过的设备」。
    localStorage.clear();
    expect(mount({ isMobile: false }).result.current.fontScale).toBe("standard");
  });

  it("prefers the cached tier over the platform default", () => {
    // 首屏缓存先渲染，避免闪一下默认档再跳变（与主题同理）。
    localStorage.setItem(FONT_SCALE_STORAGE_KEY, "large");
    expect(mount({ isMobile: true }).result.current.fontScale).toBe("large");
  });

  it("writes the phone key on a phone and the desktop key on the desktop", () => {
    // 两个键共处一个 preferences.json（配对手机写的是桌面主机那份），
    // 写错键就会一端改字号、另一端观感跟着变。
    const phone = okRpc();
    const phoneHook = mount({ isMobile: true, rpc: phone });
    act(() => phoneHook.result.current.changeFontScale("large"));
    expect(phone.calls).toContainEqual({
      method: "ui/settings/write",
      params: { font_scale_ios: "large" },
    });

    const desktop = okRpc();
    const desktopHook = mount({ isMobile: false, rpc: desktop });
    act(() => desktopHook.result.current.changeFontScale("large"));
    expect(desktop.calls).toContainEqual({
      method: "ui/settings/write",
      params: { font_scale_desktop: "large" },
    });
  });

  it("applies the tier to <html> and caches it", () => {
    const hook = mount({ isMobile: true });
    act(() => hook.result.current.changeFontScale("large"));
    expect(document.documentElement.dataset.fontScale).toBe("large");
    expect(localStorage.getItem(FONT_SCALE_STORAGE_KEY)).toBe("large");
  });

  it("rolls back to the confirmed tier when the write fails", async () => {
    // 乐观更新后写失败必须回退，否则界面停在一个服务端并不认可的档位上，
    // 下次启动就"自己变回去"——与主题的失败处理同一口径。
    const setCurrentError = vi.fn();
    const failing = {
      request: () => Promise.reject(new Error("boom")),
    };
    const hook = renderHook(() =>
      useSettingsState({
        rpc: () => failing as never,
        force: vi.fn(),
        setCurrentError,
        isMobile: true,
      }),
    );
    // 等 rejection 落地：回退发生在 `.then` 的失败回调里，不在 changeFontScale
    // 返回的那一刻。
    await act(async () => {
      hook.result.current.changeFontScale("large");
      await Promise.resolve();
    });
    expect(hook.result.current.fontScale).toBe("small");
    expect(setCurrentError).toHaveBeenCalled();
  });

  it("lets the first read adopt the stored value only until the user touches it", () => {
    // 与主题同款「触碰标记」：首屏 read 在途期间用户已选了档，迟到的旧值
    // 不得把它覆盖回原样。
    const hook = mount({ isMobile: true });
    act(() => hook.result.current.changeFontScale("large"));

    act(() => {
      const stored: FontScale = "small";
      hook.result.current.setFontScaleFromServer(stored);
    });
    expect(hook.result.current.fontScale).toBe("large");
  });
});
