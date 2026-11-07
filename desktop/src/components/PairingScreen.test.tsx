/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import {
  CONNECT_FAILED_TEXT,
  INVALID_CODE_TEXT,
  PairingScreen,
} from "./PairingScreen";
import {
  REMOTE_STORAGE_KEY,
  type StorageLike,
  storedRemoteConfig,
} from "../lib/remoteConfig";
import type { ScanResultHandler } from "../lib/qrScanner";

/** In-memory stand-in for `localStorage`; same shape `remoteConfig.test.ts` uses. */
function storages(entries: Record<string, string> = {}): StorageLike {
  const map = new Map(Object.entries(entries));
  return {
    getItem: (k: string) => map.get(k) ?? null,
    setItem: (k: string, v: string) => void map.set(k, v),
    removeItem: (k: string) => void map.delete(k),
  };
}

const PAIRED = { device_id: "dev-1", token: "yia_tok", scope: "control" };
const URL = "wss://relay.test/ws?session=abc";

/** The three fields + submit button, resolved by their visible labels. */
function fields() {
  return {
    url: screen.getByLabelText("服务器地址") as HTMLInputElement,
    code: screen.getByLabelText("配对码") as HTMLInputElement,
    name: screen.getByLabelText("设备名称") as HTMLInputElement,
    submit: screen.getByRole("button", { name: "配对" }) as HTMLButtonElement,
  };
}

function fill(
  f: ReturnType<typeof fields>,
  { code = "ABCD-EFGH", name = "我的 iPhone" } = {},
) {
  fireEvent.change(f.url, { target: { value: URL } });
  fireEvent.change(f.code, { target: { value: code } });
  fireEvent.change(f.name, { target: { value: name } });
}

afterEach(cleanup);

describe("PairingScreen validation", () => {
  it("keeps submit disabled until both the address and the code are filled", () => {
    render(<PairingScreen redeem={vi.fn()} storage={storages()} />);
    const f = fields();
    expect(f.submit.disabled).toBe(true);

    fireEvent.change(f.url, { target: { value: URL } });
    expect(f.submit.disabled).toBe(true); // code still empty

    fireEvent.change(f.code, { target: { value: "ABCD-EFGH" } });
    expect(f.submit.disabled).toBe(false);

    // Whitespace does not count as a filled field.
    fireEvent.change(f.code, { target: { value: "   " } });
    expect(f.submit.disabled).toBe(true);
  });

  it("prefills the address from `initialUrl`", () => {
    render(
      <PairingScreen redeem={vi.fn()} storage={storages()} initialUrl={URL} />,
    );
    expect(fields().url.value).toBe(URL);
  });

  it("never calls the redeemer while disabled", () => {
    const redeem = vi.fn();
    render(<PairingScreen redeem={redeem} storage={storages()} />);
    fireEvent.click(fields().submit);
    expect(redeem).not.toHaveBeenCalled();
  });
});

describe("PairingScreen submit", () => {
  it("redeems with (url, code, deviceName), persists the token and reports completion", async () => {
    const storage = storages();
    const redeem = vi.fn().mockResolvedValue(PAIRED);
    const onPaired = vi.fn();
    render(
      <PairingScreen redeem={redeem} storage={storage} onPaired={onPaired} />,
    );

    fill(fields());
    fireEvent.click(fields().submit);

    await waitFor(() => expect(onPaired).toHaveBeenCalledTimes(1));
    expect(redeem).toHaveBeenCalledWith(URL, "ABCD-EFGH", "我的 iPhone");
    // The persisted binding is what `transportFactory` reads on the next boot,
    // so it must carry the relay url under the key `storedRemoteConfig` reads.
    // `scope` 一并落盘：旁支要据此决定本设备能否开/发/停/关。
    expect(storedRemoteConfig(storage)).toEqual({
      url: URL,
      token: "yia_tok",
      projects: [],
      scope: PAIRED.scope,
    });
    expect(JSON.parse(storage.getItem(REMOTE_STORAGE_KEY)!).url).toBe(URL);
  });

  it("trims the inputs before redeeming and persisting", async () => {
    const storage = storages();
    const redeem = vi.fn().mockResolvedValue(PAIRED);
    render(<PairingScreen redeem={redeem} storage={storage} />);

    const f = fields();
    fireEvent.change(f.url, { target: { value: `  ${URL}  ` } });
    fireEvent.change(f.code, { target: { value: " ABCD-EFGH " } });
    fireEvent.change(f.name, { target: { value: " 我的 iPhone " } });
    fireEvent.click(f.submit);

    await waitFor(() => expect(redeem).toHaveBeenCalledTimes(1));
    expect(redeem).toHaveBeenCalledWith(URL, "ABCD-EFGH", "我的 iPhone");
    // `scope` 一并落盘：旁支要据此决定本设备能否开/发/停/关。
    expect(storedRemoteConfig(storage)).toEqual({
      url: URL,
      token: "yia_tok",
      projects: [],
      scope: PAIRED.scope,
    });
  });

  it("keeps the persisted config untouched when the redeem fails", async () => {
    const storage = storages();
    const redeem = vi.fn().mockRejectedValue(new Error("pairing socket error"));
    render(<PairingScreen redeem={redeem} storage={storage} />);

    fill(fields());
    fireEvent.click(fields().submit);

    await waitFor(() => expect(screen.getByRole("alert")).toBeTruthy());
    expect(storage.getItem(REMOTE_STORAGE_KEY)).toBeNull();
  });

  it("disables submit and shows a pending label while the redeem is in flight", async () => {
    let resolve!: (d: typeof PAIRED) => void;
    const redeem = vi.fn(
      () => new Promise<typeof PAIRED>((r) => (resolve = r)),
    );
    render(<PairingScreen redeem={redeem} storage={storages()} />);

    fill(fields());
    fireEvent.click(fields().submit);

    await waitFor(() =>
      expect(
        (screen.getByRole("button", { name: "配对中…" }) as HTMLButtonElement)
          .disabled,
      ).toBe(true),
    );
    expect(redeem).toHaveBeenCalledTimes(1);

    resolve(PAIRED);
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "配对" })).toBeTruthy(),
    );
  });
});

describe("PairingScreen error mapping", () => {
  it("renders the invalid-code message for a 4401 close", async () => {
    const redeem = vi
      .fn()
      .mockRejectedValue(
        new Error("pairing rejected: invalid or used code (4401)"),
      );
    render(<PairingScreen redeem={redeem} storage={storages()} />);

    fill(fields());
    fireEvent.click(fields().submit);

    await waitFor(() =>
      expect(screen.getByRole("alert").textContent).toBe(INVALID_CODE_TEXT),
    );
  });

  it("also recognises a structured 4401 rejection", async () => {
    const redeem = vi
      .fn()
      .mockRejectedValue({ code: 4401, message: "no token" });
    render(<PairingScreen redeem={redeem} storage={storages()} />);

    fill(fields());
    fireEvent.click(fields().submit);

    await waitFor(() =>
      expect(screen.getByRole("alert").textContent).toBe(INVALID_CODE_TEXT),
    );
  });

  // 中继路径（`?session=…`）不走 4401 关帧：`pair/redeem` 作为 JSON-RPC 调用失败，
  // 服务端返回 -32001「invalid or expired pairing code」。若只认 4401，过期/用过的码
  // 就会被误报成「无法连接服务器」，把「重新生成码」引向「排查网络」的死路。
  it("recognises the relay's -32001 invalid-code rejection", async () => {
    const redeem = vi.fn().mockRejectedValue({
      code: -32001,
      message: "invalid or expired pairing code",
    });
    render(<PairingScreen redeem={redeem} storage={storages()} />);

    fill(fields());
    fireEvent.click(fields().submit);

    await waitFor(() =>
      expect(screen.getByRole("alert").textContent).toBe(INVALID_CODE_TEXT),
    );
  });

  it("maps every other failure to the connect message", async () => {
    const redeem = vi
      .fn()
      .mockRejectedValue(new Error("pairing socket closed (1006)"));
    render(<PairingScreen redeem={redeem} storage={storages()} />);

    fill(fields());
    fireEvent.click(fields().submit);

    await waitFor(() =>
      expect(screen.getByRole("alert").textContent).toBe(CONNECT_FAILED_TEXT),
    );
  });
});

describe("PairingScreen host seam", () => {
  it("delegates the whole submit to `onSubmit` when the host injects one", async () => {
    const onSubmit = vi.fn().mockResolvedValue(undefined);
    const redeem = vi.fn();
    render(
      <PairingScreen
        redeem={redeem}
        onSubmit={onSubmit}
        storage={storages()}
      />,
    );

    fill(fields());
    fireEvent.click(fields().submit);

    await waitFor(() =>
      expect(onSubmit).toHaveBeenCalledWith(URL, "ABCD-EFGH", "我的 iPhone"),
    );
    expect(redeem).not.toHaveBeenCalled();
  });
});

describe("PairingScreen QR scan", () => {
  it("auto-pairs when the scanner delivers a valid payload", async () => {
    const redeem = vi.fn(async () => PAIRED);
    const storage = storages();
    const scan = (onText: (t: string) => void) => {
      onText("yiagent://pair?v=1&relay=wss%3A%2F%2Fr%2Fws&code=ABCD-EFGH");
      return () => {};
    };
    render(
      <PairingScreen
        enableScan
        redeem={redeem}
        storage={storage}
        scan={scan}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "扫码" }));

    await waitFor(() =>
      expect(redeem).toHaveBeenCalledWith("wss://r/ws", "ABCD-EFGH", "iPhone"),
    );
    expect(storedRemoteConfig(storage)).toEqual({
      url: "wss://r/ws",
      token: "yia_tok",
      projects: [],
      scope: PAIRED.scope,
    });
  });

  it("shows a message and does not redeem when the payload is not a pairing QR", async () => {
    const redeem = vi.fn();
    const scan = (onText: (t: string) => void) => {
      onText("https://example.com/not-a-pairing-code");
      return () => {};
    };
    render(
      <PairingScreen
        enableScan
        redeem={redeem}
        scan={scan}
        storage={storages()}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "扫码" }));

    await waitFor(() =>
      expect(screen.getByRole("alert").textContent).toBe(
        "不是有效的配对二维码",
      ),
    );
    expect(redeem).not.toHaveBeenCalled();
  });

  it("falls back to manual entry when the camera is unavailable", async () => {
    const scan = (
      _onText: (t: string) => void,
      onError: (e: unknown) => void,
    ) => {
      onError(new Error("no camera"));
      return () => {};
    };
    render(<PairingScreen enableScan scan={scan} storage={storages()} />);

    fireEvent.click(screen.getByRole("button", { name: "扫码" }));

    await waitFor(() =>
      expect(screen.getByRole("alert").textContent).toBe(
        "无法访问相机，可手输配对码",
      ),
    );
    expect(screen.getByLabelText("配对码")).toBeTruthy(); // 表单仍可用
  });

  it("hides the scan button when scanning is not enabled", () => {
    render(
      <PairingScreen
        enableScan={false}
        redeem={vi.fn()}
        storage={storages()}
      />,
    );
    expect(screen.queryByRole("button", { name: "扫码" })).toBeNull();
  });

  it("lets the user cancel the scanner and return to the form", async () => {
    const scan = () => () => {};
    render(
      <PairingScreen
        enableScan
        scan={scan}
        redeem={vi.fn()}
        storage={storages()}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "扫码" }));
    expect(screen.getByRole("button", { name: "返回" })).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "返回" }));

    await waitFor(() =>
      expect(screen.queryByRole("button", { name: "返回" })).toBeNull(),
    );
    expect(screen.getByLabelText("配对码")).toBeTruthy(); // 回到可编辑表单
  });

  it("keeps the scanner's exit inside the phone safe area", () => {
    // 覆盖层是 `fixed inset-0`，脱离 `body` 的安全区内边距（仓库在 index.css 里
    // 反复记过这条）。头部若不自己补 `env(safe-area-inset-top)`，唯一的出口就会
    // 落在刘海/灵动岛底下——用户报的「扫码过程中没有返回按钮」。
    const scan = () => () => {};
    render(
      <PairingScreen
        enableScan
        scan={scan}
        redeem={vi.fn()}
        storage={storages()}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: "扫码" }));

    const dialog = screen.getByRole("dialog", { name: "扫描二维码" });
    const bar = dialog.querySelector(".scan-overlay-bar");
    expect(bar).toBeTruthy(); // CSS 在 [data-mobile] 下给它补 padding-top: max(env(safe-area-inset-top…))
    expect(bar?.contains(screen.getByRole("button", { name: "返回" }))).toBe(
      true,
    );
  });

  it("returns to the form instead of blanking the app when the scanner throws", async () => {
    // 相机在 iOS 上的失败方式很杂；任何一条变成渲染期异常，React 19 都会卸载整棵树
    // ——手机 body 底色是深色，呈现出来就是「黑屏」。ErrorBoundary 必须把它就地
    // 收敛成「回表单 + 说明原因」。
    const spy = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      const scan = () => {
        throw new Error("camera exploded");
      };
      render(
        <PairingScreen
          enableScan
          scan={scan}
          redeem={vi.fn()}
          storage={storages()}
        />,
      );

      fireEvent.click(screen.getByRole("button", { name: "扫码" }));

      await waitFor(() =>
        expect(screen.getByRole("alert").textContent).toBe(
          "无法访问相机，可手输配对码",
        ),
      );
      expect(screen.queryByRole("dialog", { name: "扫描二维码" })).toBeNull();
      expect(screen.getByLabelText("配对码")).toBeTruthy();
    } finally {
      spy.mockRestore();
    }
  });

  it("shows the invalid-QR message over the scanner (not hidden behind it)", async () => {
    const scan = (onText: (t: string) => void) => {
      onText("https://example.com/nope");
      return () => {};
    };
    render(
      <PairingScreen
        enableScan
        scan={scan}
        redeem={vi.fn()}
        storage={storages()}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "扫码" }));

    // 错误必须渲染在扫描器覆盖层**内部**，否则会被 z-50 的黑色层盖住、用户看不到。
    await waitFor(() =>
      expect(screen.getByRole("alert").textContent).toBe(
        "不是有效的配对二维码",
      ),
    );
    const dialog = screen.getByRole("dialog", { name: "扫描二维码" });
    expect(dialog.contains(screen.getByRole("alert"))).toBe(true);
    expect(screen.getByRole("button", { name: "返回" })).toBeTruthy(); // 扫描器仍开着
  });
});

describe("PairingScreen optional cancel", () => {
  it("renders no cancel button on the first-launch path", () => {
    // 首启不传 `onCancel`：DOM 必须与从前一致（QR 配对的硬不变量）。
    render(<PairingScreen redeem={vi.fn()} storage={storages()} />);
    expect(screen.queryByRole("button", { name: "返回" })).toBeNull();
  });

  it("renders a working cancel button when the host passes one", () => {
    const onCancel = vi.fn();
    render(
      <PairingScreen
        redeem={vi.fn()}
        storage={storages()}
        onCancel={onCancel}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: "返回" }));
    expect(onCancel).toHaveBeenCalledTimes(1);
  });
});


/** A promise whose settlement the test controls (Task 1/2 deferred shape). */
function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

function scanPayload(code = "ABCD-EFGH") {
  return `yiagent://pair?v=1&relay=${encodeURIComponent(URL)}&code=${code}`;
}

/** Injects a scan seam that hands the test the live result handler. */
function captureScan() {
  const box: { emit?: ScanResultHandler; error?: (e: unknown) => void } = {};
  const scan = vi.fn((onText: ScanResultHandler, onError: (e: unknown) => void) => {
    box.emit = onText;
    box.error = onError;
    return vi.fn();
  });
  return { box, scan };
}

describe("PairingScreen pairing admission", () => {
  it("A01 accepted QR removes overlay while redeem is pending", async () => {
    const pending = deferred<typeof PAIRED>();
    const redeem = vi.fn(() => pending.promise);
    const { box, scan } = captureScan();
    render(
      <PairingScreen
        enableScan
        scan={scan}
        redeem={redeem}
        storage={storages()}
        notice="已被桌面移除"
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: "扫码" }));
    let accepted: boolean | void = undefined;
    act(() => {
      accepted = box.emit!(scanPayload());
    });
    // 先检查用户可见行为：旧实现覆盖层残留，会先红在这里。
    expect(
      screen.queryByRole("dialog", { name: "扫描二维码" }),
    ).toBeNull();
    expect(accepted).toBe(true);
    expect(
      screen.getByRole("status", { name: "配对进度" }).textContent,
    ).toBe("已识别二维码，正在连接桌面…");
    expect(
      (screen.getByLabelText("服务器地址") as HTMLInputElement).value,
    ).toBe(URL);
    expect(redeem).toHaveBeenCalledTimes(1);
    await act(async () => {
      pending.reject(new Error("offline"));
    });
    expect(screen.queryByRole("status", { name: "配对进度" })).toBeNull();
    expect(screen.getByRole("alert").textContent).toBe(CONNECT_FAILED_TEXT);
  });

  it("A02 synchronous manual submit admission survives stale busy closure", async () => {
    const pending = deferred<typeof PAIRED>();
    const redeem = vi.fn(() => pending.promise);
    render(<PairingScreen enableScan storage={storages()} redeem={redeem} />);
    const f = fields();
    fill(f);
    const form = screen.getByRole("form", { name: "配对" });
    act(() => {
      fireEvent.submit(form);
      fireEvent.submit(form); // 同一 React 批次，busy 闭包尚未提交
      fireEvent.change(f.url, { target: { value: "wss://other.test/ws" } });
      fireEvent.change(f.code, { target: { value: "ZZZZ-YYYY" } });
      fireEvent.change(f.name, { target: { value: "other phone" } });
    });
    expect(redeem).toHaveBeenCalledTimes(1);
    expect([f.url.value, f.code.value, f.name.value]).toEqual([
      URL,
      "ABCD-EFGH",
      "我的 iPhone",
    ]);
    expect([f.url.disabled, f.code.disabled, f.name.disabled]).toEqual([
      true,
      true,
      true,
    ]);
    fireEvent.submit(form); // pending 全周期仍不得重新准入
    expect(redeem).toHaveBeenCalledTimes(1);
    expect(screen.queryByRole("button", { name: "扫码" })).toBeNull();
    await act(async () => {
      pending.reject(new Error("offline"));
    });
    expect([f.url.disabled, f.code.disabled, f.name.disabled]).toEqual([
      false,
      false,
      false,
    ]);
  });

  it("A02/A18 first accepted code wins and later frames are not forwarded", async () => {
    const pending = deferred<typeof PAIRED>();
    const redeem = vi.fn(() => pending.promise);
    const { box, scan } = captureScan();
    render(<PairingScreen enableScan scan={scan} redeem={redeem} storage={storages()} />);
    const f = fields(); // busy 后按钮名变为“配对中…”，必须提前缓存
    fireEvent.click(screen.getByRole("button", { name: "扫码" }));
    const results: (boolean | void)[] = [];
    act(() => {
      results.push(box.emit!("https://example.com/not-a-pairing-code"));
      results.push(box.emit!(scanPayload()));
      results.push(box.emit!(scanPayload()));
      results.push(box.emit!(scanPayload("ZZZZ-YYYY")));
      results.push(box.emit!("garbage"));
    });
    expect(results).toEqual([false, true, true, true, true]);
    expect(redeem).toHaveBeenCalledTimes(1);
    expect([f.url.value, f.code.value]).toEqual([URL, "ABCD-EFGH"]);
    expect(screen.queryByRole("alert")).toBeNull();
    expect(f.url.disabled).toBe(true);
    await act(async () => {
      pending.resolve(PAIRED);
    });
  });

  it("A03 invalid and network failures restore the form and keep the code", async () => {
    const redeem = vi
      .fn()
      .mockRejectedValueOnce({ code: 4401, message: "no token" })
      .mockRejectedValueOnce({ code: -32001, message: "invalid or expired pairing code" })
      .mockRejectedValueOnce(new Error("pairing socket closed (1006)"))
      .mockResolvedValueOnce(PAIRED);
    const onPaired = vi.fn();
    render(
      <PairingScreen redeem={redeem} storage={storages()} onPaired={onPaired} />,
    );
    const f = fields();
    for (const expected of [
      INVALID_CODE_TEXT,
      INVALID_CODE_TEXT,
      CONNECT_FAILED_TEXT,
    ]) {
      fill(f);
      fireEvent.click(f.submit);
      await waitFor(() =>
        expect(screen.getByRole("alert").textContent).toBe(expected),
      );
      expect(screen.queryByRole("status", { name: "配对进度" })).toBeNull();
      expect([f.url.disabled, f.code.disabled, f.name.disabled]).toEqual([
        false,
        false,
        false,
      ]);
      expect([f.url.value, f.code.value]).toEqual([URL, "ABCD-EFGH"]);
    }
    fill(f);
    fireEvent.click(f.submit);
    await waitFor(() => expect(onPaired).toHaveBeenCalledTimes(1));
  });

  it("A04 host onSubmit bypasses storage and is notified once", async () => {
    const setItem = vi.fn();
    const storage: StorageLike = { ...storages(), setItem };
    const redeem = vi.fn();
    const onSubmit = vi.fn().mockResolvedValue(undefined);
    const onPaired = vi.fn();
    render(
      <PairingScreen
        onSubmit={onSubmit}
        redeem={redeem}
        storage={storage}
        onPaired={onPaired}
      />,
    );
    fill(fields());
    fireEvent.click(fields().submit);
    await waitFor(() => expect(onPaired).toHaveBeenCalledTimes(1));
    expect(onSubmit).toHaveBeenCalledWith(URL, "ABCD-EFGH", "我的 iPhone");
    expect(redeem).not.toHaveBeenCalled();
    expect(setItem).not.toHaveBeenCalled();
  });

  it("A04 persisted config is visible before onPaired fires (once)", async () => {
    const storage = storages();
    const redeem = vi.fn().mockResolvedValue({ ...PAIRED, scope: "observe" });
    const seen: unknown[] = [];
    const onPaired = vi.fn(() => {
      seen.push(storedRemoteConfig(storage));
    });
    render(
      <PairingScreen redeem={redeem} storage={storage} onPaired={onPaired} />,
    );
    fill(fields());
    fireEvent.click(fields().submit);
    await waitFor(() => expect(onPaired).toHaveBeenCalledTimes(1));
    expect(seen).toEqual([
      { url: URL, token: "yia_tok", scope: "observe", projects: [] },
    ]);
    expect(storedRemoteConfig(storage)).toEqual(seen[0]);
  });

  it("A15 completed scan leaves no overlay and reports the source text", async () => {
    const storage = storages();
    const onPaired = vi.fn();
    const { box, scan } = captureScan();
    const view = render(
      <PairingScreen
        enableScan
        scan={scan}
        redeem={vi.fn().mockResolvedValue(PAIRED)}
        storage={storage}
        onPaired={onPaired}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: "扫码" }));
    await act(async () => {
      box.emit!(scanPayload());
    });
    expect(screen.queryByRole("dialog", { name: "扫描二维码" })).toBeNull();
    expect(onPaired).toHaveBeenCalledTimes(1);
    expect(storedRemoteConfig(storage)?.token).toBe("yia_tok");
    view.unmount();

    const failing = captureScan();
    render(
      <PairingScreen
        enableScan
        scan={failing.scan}
        redeem={vi.fn().mockRejectedValue(new Error("offline"))}
        storage={storages()}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: "扫码" }));
    await act(async () => {
      failing.box.emit!(scanPayload());
    });
    expect(screen.queryByRole("dialog", { name: "扫描二维码" })).toBeNull();
    expect(screen.getByRole("alert").textContent).toBe(CONNECT_FAILED_TEXT);
  });

  it("A15 manual pending shows its own progress text and no cancel reminder by default", async () => {
    const pending = deferred<typeof PAIRED>();
    render(
      <PairingScreen
        enableScan
        storage={storages()}
        redeem={() => pending.promise}
      />,
    );
    const f = fields();
    fill(f);
    fireEvent.click(f.submit);
    expect(
      screen.getByRole("status", { name: "配对进度" }).textContent,
    ).toBe("正在连接桌面…");
    expect(screen.queryByText("返回不会撤销已发出的配对请求")).toBeNull();
    await act(async () => {
      pending.resolve(PAIRED);
    });
  });

  const leaveCases = [
    ["return-without-unmount", "resolve"],
    ["return-without-unmount", "reject"],
    ["unmount-new-page", "resolve"],
    ["unmount-new-page", "reject"],
  ] as const;

  it.each(leaveCases)(
    "A16 %s ignores late %s",
    async (leave, outcome) => {
      const oldRaw = JSON.stringify(
        { url: URL, token: "yia_old", scope: "observe", projects: [] },
        null,
        2,
      );
      const storage = storages({ [REMOTE_STORAGE_KEY]: oldRaw });
      expect(storedRemoteConfig(storage)?.token).toBe("yia_old");
      const pending = deferred<typeof PAIRED>();
      const onPaired = vi.fn();
      const onCancel = vi.fn();
      const view = render(
        <PairingScreen
          enableScan={false}
          storage={storage}
          redeem={() => pending.promise}
          onPaired={onPaired}
          onCancel={onCancel}
        />,
      );
      const f = fields(); // busy 后按钮名变为“配对中…”，必须在 busy 前缓存
      fill(f);
      fireEvent.click(f.submit);
      expect(screen.getByText("返回不会撤销已发出的配对请求")).toBeTruthy();
      if (leave === "return-without-unmount") {
        fireEvent.click(screen.getByRole("button", { name: "返回" }));
        expect(onCancel).toHaveBeenCalledTimes(1);
        // 宿主尚未卸载：必须由返回 handler 自身失效，不能靠 cleanup 掩盖。
      } else {
        view.unmount();
        render(<PairingScreen storage={storage} enableScan={false} />);
      }
      await act(async () => {
        if (outcome === "resolve") pending.resolve(PAIRED);
        else pending.reject(new Error("old request failed"));
      });
      expect(storage.getItem(REMOTE_STORAGE_KEY)).toBe(oldRaw);
      expect(onPaired).not.toHaveBeenCalled();
      expect(screen.queryByRole("alert")).toBeNull();
    },
  );

  it("A18 stale handlers after returning cannot touch the reopened scanner", async () => {
    const redeem = vi.fn().mockResolvedValue(PAIRED);
    const { box, scan } = captureScan();
    render(
      <PairingScreen enableScan scan={scan} redeem={redeem} storage={storages()} />,
    );
    fireEvent.click(screen.getByRole("button", { name: "扫码" }));
    const staleText = box.emit!;
    const staleError = box.error!;
    fireEvent.click(screen.getByRole("button", { name: "返回" }));
    let staleResult: boolean | void = undefined;
    act(() => {
      staleResult = staleText("https://example.com/stale");
      staleError(new Error("stale camera"));
    });
    expect(staleResult).toBe(true);
    expect(screen.queryByRole("alert")).toBeNull();
    expect(redeem).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("button", { name: "扫码" }));
    const freshText = box.emit!;
    act(() => {
      staleText(scanPayload());
      staleError(new Error("stale camera again"));
    });
    expect(redeem).not.toHaveBeenCalled();
    expect(screen.queryByRole("alert")).toBeNull();
    act(() => {
      freshText(scanPayload());
      staleError(new Error("stale after accept"));
    });
    expect(redeem).toHaveBeenCalledTimes(1);
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("A18 injected scan reporting two errors handles only the first", async () => {
    const scan = vi.fn(
      (_onText: ScanResultHandler, onError: (e: unknown) => void) => {
        onError(new Error("first camera failure"));
        onError(new Error("second camera failure"));
        return vi.fn();
      },
    );
    render(<PairingScreen enableScan scan={scan} storage={storages()} />);
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "扫码" }));
    });
    expect(screen.getByRole("alert").textContent).toBe(
      "无法访问相机，可手输配对码",
    );
    expect(screen.queryByRole("dialog", { name: "扫描二维码" })).toBeNull();
  });
});
