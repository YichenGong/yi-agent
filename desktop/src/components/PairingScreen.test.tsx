/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { CONNECT_FAILED_TEXT, INVALID_CODE_TEXT, PairingScreen } from "./PairingScreen";
import {
  REMOTE_STORAGE_KEY,
  type StorageLike,
  storedRemoteConfig,
} from "../lib/remoteConfig";

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

function fill(f: ReturnType<typeof fields>, { code = "ABCD-EFGH", name = "我的 iPhone" } = {}) {
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
    render(<PairingScreen redeem={vi.fn()} storage={storages()} initialUrl={URL} />);
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
    render(<PairingScreen redeem={redeem} storage={storage} onPaired={onPaired} />);

    fill(fields());
    fireEvent.click(fields().submit);

    await waitFor(() => expect(onPaired).toHaveBeenCalledTimes(1));
    expect(redeem).toHaveBeenCalledWith(URL, "ABCD-EFGH", "我的 iPhone");
    // The persisted binding is what `transportFactory` reads on the next boot,
    // so it must carry the relay url under the key `storedRemoteConfig` reads.
    expect(storedRemoteConfig(storage)).toEqual({ url: URL, token: "yia_tok" });
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
    expect(storedRemoteConfig(storage)).toEqual({ url: URL, token: "yia_tok" });
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
    const redeem = vi.fn(() => new Promise<typeof PAIRED>((r) => (resolve = r)));
    render(<PairingScreen redeem={redeem} storage={storages()} />);

    fill(fields());
    fireEvent.click(fields().submit);

    await waitFor(() =>
      expect((screen.getByRole("button", { name: "配对中…" }) as HTMLButtonElement).disabled).toBe(
        true,
      ),
    );
    expect(redeem).toHaveBeenCalledTimes(1);

    resolve(PAIRED);
    await waitFor(() => expect(screen.getByRole("button", { name: "配对" })).toBeTruthy());
  });
});

describe("PairingScreen error mapping", () => {
  it("renders the invalid-code message for a 4401 close", async () => {
    const redeem = vi
      .fn()
      .mockRejectedValue(new Error("pairing rejected: invalid or used code (4401)"));
    render(<PairingScreen redeem={redeem} storage={storages()} />);

    fill(fields());
    fireEvent.click(fields().submit);

    await waitFor(() => expect(screen.getByRole("alert").textContent).toBe(INVALID_CODE_TEXT));
  });

  it("also recognises a structured 4401 rejection", async () => {
    const redeem = vi.fn().mockRejectedValue({ code: 4401, message: "no token" });
    render(<PairingScreen redeem={redeem} storage={storages()} />);

    fill(fields());
    fireEvent.click(fields().submit);

    await waitFor(() => expect(screen.getByRole("alert").textContent).toBe(INVALID_CODE_TEXT));
  });

  it("maps every other failure to the connect message", async () => {
    const redeem = vi.fn().mockRejectedValue(new Error("pairing socket closed (1006)"));
    render(<PairingScreen redeem={redeem} storage={storages()} />);

    fill(fields());
    fireEvent.click(fields().submit);

    await waitFor(() => expect(screen.getByRole("alert").textContent).toBe(CONNECT_FAILED_TEXT));
  });
});

describe("PairingScreen host seam", () => {
  it("delegates the whole submit to `onSubmit` when the host injects one", async () => {
    const onSubmit = vi.fn().mockResolvedValue(undefined);
    const redeem = vi.fn();
    render(<PairingScreen redeem={redeem} onSubmit={onSubmit} storage={storages()} />);

    fill(fields());
    fireEvent.click(fields().submit);

    await waitFor(() => expect(onSubmit).toHaveBeenCalledWith(URL, "ABCD-EFGH", "我的 iPhone"));
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
    render(<PairingScreen enableScan redeem={redeem} storage={storage} scan={scan} />);

    fireEvent.click(screen.getByRole("button", { name: "扫码" }));

    await waitFor(() => expect(redeem).toHaveBeenCalledWith("wss://r/ws", "ABCD-EFGH", "iPhone"));
    expect(storedRemoteConfig(storage)).toEqual({ url: "wss://r/ws", token: "yia_tok" });
  });

  it("shows a message and does not redeem when the payload is not a pairing QR", async () => {
    const redeem = vi.fn();
    const scan = (onText: (t: string) => void) => {
      onText("https://example.com/not-a-pairing-code");
      return () => {};
    };
    render(<PairingScreen enableScan redeem={redeem} scan={scan} storage={storages()} />);

    fireEvent.click(screen.getByRole("button", { name: "扫码" }));

    await waitFor(() =>
      expect(screen.getByRole("alert").textContent).toBe("不是有效的配对二维码"),
    );
    expect(redeem).not.toHaveBeenCalled();
  });

  it("falls back to manual entry when the camera is unavailable", async () => {
    const scan = (_onText: (t: string) => void, onError: (e: unknown) => void) => {
      onError(new Error("no camera"));
      return () => {};
    };
    render(<PairingScreen enableScan scan={scan} storage={storages()} />);

    fireEvent.click(screen.getByRole("button", { name: "扫码" }));

    await waitFor(() =>
      expect(screen.getByRole("alert").textContent).toBe("无法访问相机，可手输配对码"),
    );
    expect(screen.getByLabelText("配对码")).toBeTruthy(); // 表单仍可用
  });

  it("hides the scan button when scanning is not enabled", () => {
    render(<PairingScreen enableScan={false} redeem={vi.fn()} storage={storages()} />);
    expect(screen.queryByRole("button", { name: "扫码" })).toBeNull();
  });

  it("lets the user cancel the scanner and return to the form", async () => {
    const scan = () => () => {};
    render(<PairingScreen enableScan scan={scan} redeem={vi.fn()} storage={storages()} />);

    fireEvent.click(screen.getByRole("button", { name: "扫码" }));
    expect(screen.getByRole("button", { name: "取消" })).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "取消" }));

    await waitFor(() => expect(screen.queryByRole("button", { name: "取消" })).toBeNull());
    expect(screen.getByLabelText("配对码")).toBeTruthy(); // 回到可编辑表单
  });

  it("shows the invalid-QR message over the scanner (not hidden behind it)", async () => {
    const scan = (onText: (t: string) => void) => {
      onText("https://example.com/nope");
      return () => {};
    };
    render(<PairingScreen enableScan scan={scan} redeem={vi.fn()} storage={storages()} />);

    fireEvent.click(screen.getByRole("button", { name: "扫码" }));

    // 错误必须渲染在扫描器覆盖层**内部**，否则会被 z-50 的黑色层盖住、用户看不到。
    await waitFor(() =>
      expect(screen.getByRole("alert").textContent).toBe("不是有效的配对二维码"),
    );
    const dialog = screen.getByRole("dialog", { name: "扫描二维码" });
    expect(dialog.contains(screen.getByRole("alert"))).toBe(true);
    expect(screen.getByRole("button", { name: "取消" })).toBeTruthy(); // 扫描器仍开着
  });
});
