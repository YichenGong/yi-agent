/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import QRCode from "qrcode";
import { ADMIN_REQUIRED_TEXT, SettingsRemoteTab } from "./SettingsRemoteTab";
import { parsePairUri } from "../lib/pairUri";

afterEach(cleanup);

const DEVICES = [
  { id: "d1", name: "iPhone 15", scope: "control", created_at: 1, last_seen_at: 2 },
  { id: "d2", name: "iPad Air", scope: "control", created_at: 3, last_seen_at: 4 },
];

/** 假 RPC：按方法名派发，未登记的方法视为测试写错。 */
function rpcStub(handlers: Record<string, (params: unknown) => unknown>) {
  return vi.fn(async (method: string, params: unknown) => {
    const handler = handlers[method];
    if (!handler) throw new Error(`unexpected method ${method}`);
    return handler(params);
  });
}

describe("SettingsRemoteTab", () => {
  it("mints a pairing code and renders it", async () => {
    const call = rpcStub({
      "device/list": () => ({ devices: [] }),
      "pair/create": () => ({ code: "ABCD-EFGH", expires_in: 300 }),
    });
    render(<SettingsRemoteTab call={call} />);

    fireEvent.click(await screen.findByRole("button", { name: "生成配对码" }));

    expect(await screen.findByText("ABCD-EFGH")).toBeTruthy();
    expect(call).toHaveBeenCalledWith("pair/create", {});
  });

  it("counts the code lifetime down and clears the interval on unmount", async () => {
    vi.useFakeTimers();
    try {
      const call = rpcStub({
        "device/list": () => ({ devices: [] }),
        "pair/create": () => ({ code: "ABCD-EFGH", expires_in: 300 }),
      });
      const { unmount } = render(<SettingsRemoteTab call={call} />);
      await act(async () => {});

      await act(async () => {
        fireEvent.click(screen.getByRole("button", { name: "生成配对码" }));
      });

      expect(screen.getByText("剩余 300 秒")).toBeTruthy();
      act(() => {
        vi.advanceTimersByTime(1000);
      });
      expect(screen.getByText("剩余 299 秒")).toBeTruthy();

      unmount();
      expect(vi.getTimerCount()).toBe(0);
    } finally {
      vi.useRealTimers();
    }
  });

  it("lists the paired devices from device/list", async () => {
    const call = rpcStub({ "device/list": () => ({ devices: DEVICES }) });
    render(<SettingsRemoteTab call={call} />);

    expect(await screen.findByText("iPhone 15")).toBeTruthy();
    expect(screen.getByText("iPad Air")).toBeTruthy();
    expect(screen.getAllByText("control")).toHaveLength(2);
    expect(call).toHaveBeenCalledWith("device/list", {});
  });

  it("revokes a device and drops its row", async () => {
    const call = rpcStub({
      "device/list": () => ({ devices: DEVICES }),
      "device/revoke": () => ({ removed: true }),
    });
    render(<SettingsRemoteTab call={call} />);

    fireEvent.click(await screen.findByRole("button", { name: "撤销 iPhone 15" }));

    await waitFor(() => expect(screen.queryByText("iPhone 15")).toBeNull());
    expect(call).toHaveBeenCalledWith("device/revoke", { device_id: "d1" });
    expect(screen.getByText("iPad Air")).toBeTruthy();
  });

  it("keeps the row when the server reports removed:false", async () => {
    const call = rpcStub({
      "device/list": () => ({ devices: DEVICES }),
      "device/revoke": () => ({ removed: false }),
    });
    render(<SettingsRemoteTab call={call} />);

    fireEvent.click(await screen.findByRole("button", { name: "撤销 iPhone 15" }));

    await waitFor(() => expect(call).toHaveBeenCalledWith("device/revoke", { device_id: "d1" }));
    expect(screen.getByText("iPhone 15")).toBeTruthy();
  });

  it("shows the permission notice when device/list is refused with -32014", async () => {
    const call = rpcStub({
      "device/list": () => {
        throw { code: -32014, message: "admin scope required" };
      },
    });
    render(<SettingsRemoteTab call={call} />);

    expect(await screen.findByText(ADMIN_REQUIRED_TEXT)).toBeTruthy();
  });

  it("shows the permission notice when pair/create is refused with -32014", async () => {
    const call = rpcStub({
      "device/list": () => ({ devices: [] }),
      "pair/create": () => {
        throw { code: -32014, message: "admin scope required" };
      },
    });
    render(<SettingsRemoteTab call={call} />);

    fireEvent.click(await screen.findByRole("button", { name: "生成配对码" }));

    expect(await screen.findByText(ADMIN_REQUIRED_TEXT)).toBeTruthy();
    expect(screen.queryByText("-32014")).toBeNull();
  });

  it("shows other failures inline with the server message", async () => {
    const call = rpcStub({
      "device/list": () => ({ devices: [] }),
      "pair/create": () => {
        throw { code: -32603, message: "internal error" };
      },
    });
    render(<SettingsRemoteTab call={call} />);

    fireEvent.click(await screen.findByRole("button", { name: "生成配对码" }));

    expect(await screen.findByText("internal error")).toBeTruthy();
  });

  it("names the relay address field for the phone operator", () => {
    render(<SettingsRemoteTab call={vi.fn()} initialRelayUrl="wss://relay.example.com/ws" />);

    const field = screen.getByLabelText("中继地址") as HTMLInputElement;
    expect(field.value).toBe("wss://relay.example.com/ws");
    fireEvent.change(field, { target: { value: "wss://other.example.com/ws" } });
    expect(field.value).toBe("wss://other.example.com/ws");
  });

  it("saves the typed relay url through the seam", async () => {
    const saveRelayUrl = vi.fn(async () => {});
    const call = rpcStub({ "device/list": () => ({ devices: [] }) });
    render(
      <SettingsRemoteTab call={call} saveRelayUrl={saveRelayUrl} initialRelayUrl="wss://old" />,
    );

    const field = screen.getByLabelText("中继地址") as HTMLInputElement;
    fireEvent.change(field, {
      target: { value: "wss://relay.example.com/connect?session=x" },
    });
    fireEvent.click(screen.getByRole("button", { name: "保存" }));

    await waitFor(() =>
      expect(saveRelayUrl).toHaveBeenCalledWith("wss://relay.example.com/connect?session=x"),
    );
  });

  it("saves null when the field is cleared", async () => {
    const saveRelayUrl = vi.fn(async () => {});
    const call = rpcStub({ "device/list": () => ({ devices: [] }) });
    render(
      <SettingsRemoteTab call={call} saveRelayUrl={saveRelayUrl} initialRelayUrl="wss://old" />,
    );

    fireEvent.change(screen.getByLabelText("中继地址"), { target: { value: "   " } });
    fireEvent.click(screen.getByRole("button", { name: "保存" }));

    await waitFor(() => expect(saveRelayUrl).toHaveBeenCalledWith(null));
  });

  it("shows the reconnecting state after a successful save", async () => {
    const saveRelayUrl = vi.fn(async () => {});
    const call = rpcStub({ "device/list": () => ({ devices: [] }) });
    render(<SettingsRemoteTab call={call} saveRelayUrl={saveRelayUrl} />);

    fireEvent.change(screen.getByLabelText("中继地址"), { target: { value: "wss://r/connect?session=x" } });
    fireEvent.click(screen.getByRole("button", { name: "保存" }));

    expect(await screen.findByText(/已保存/)).toBeTruthy();
  });

  it("reports a failed save instead of the reconnecting state", async () => {
    const saveRelayUrl = vi.fn(async () => {
      throw new Error("disk full");
    });
    const call = rpcStub({ "device/list": () => ({ devices: [] }) });
    render(<SettingsRemoteTab call={call} saveRelayUrl={saveRelayUrl} />);

    fireEvent.click(screen.getByRole("button", { name: "保存" }));

    expect(await screen.findByText("disk full")).toBeTruthy();
    expect(screen.queryByText(/已保存/)).toBeNull();
  });

  it("degrades gracefully without an RPC seam", async () => {
    render(<SettingsRemoteTab />);

    const mint = screen.getByRole("button", { name: "生成配对码" }) as HTMLButtonElement;
    expect(mint.disabled).toBe(true);
    expect(screen.getByLabelText("中继地址")).toBeTruthy();
  });

  it("renders a QR code once a code is minted and a relay url is present", async () => {
    const call = rpcStub({
      "device/list": () => ({ devices: [] }),
      "pair/create": () => ({ code: "ABCD-EFGH", expires_in: 300 }),
    });
    render(
      <SettingsRemoteTab call={call} initialRelayUrl="wss://relay.example.com/ws?session=abc" />,
    );
    fireEvent.click(await screen.findByRole("button", { name: "生成配对码" }));
    await screen.findByText("ABCD-EFGH");
    await waitFor(() => expect(screen.getByLabelText("配对二维码")).toBeTruthy());
  });

  it("encodes the phone (/ws) endpoint in the QR payload, not the desktop (/connect) one", async () => {
    // 二维码真实载荷：`buildPairUri` 不向 DOM 暴露输入，故截获编码调用的第一个参数。
    const encoded: string[] = [];
    const original = QRCode.toString.bind(QRCode);
    const spy = vi.spyOn(QRCode, "toString").mockImplementation((async (
      text: string,
      opts?: unknown,
    ) => {
      encoded.push(text);
      return original(text, opts as Parameters<typeof QRCode.toString>[1]);
    }) as typeof QRCode.toString);
    try {
      const call = rpcStub({
        "device/list": () => ({ devices: [] }),
        "pair/create": () => ({ code: "ABCD-EFGH", expires_in: 300 }),
      });
      render(
        <SettingsRemoteTab
          call={call}
          initialRelayUrl="wss://relay.example.com/connect?session=abc"
        />,
      );
      fireEvent.click(await screen.findByRole("button", { name: "生成配对码" }));
      await screen.findByText("ABCD-EFGH");
      await waitFor(() => expect(encoded.length).toBeGreaterThan(0));

      const payload = encoded[encoded.length - 1];
      expect(payload).not.toContain("%2Fconnect");
      expect(payload).toContain("%2Fws");
      expect(parsePairUri(payload)).toEqual({
        relay: "wss://relay.example.com/ws?session=abc",
        code: "ABCD-EFGH",
      });
    } finally {
      spy.mockRestore();
    }
  });

  it("falls back to the raw url in the QR when it is not a /connect endpoint", async () => {
    const encoded: string[] = [];
    const original = QRCode.toString.bind(QRCode);
    const spy = vi.spyOn(QRCode, "toString").mockImplementation((async (
      text: string,
      opts?: unknown,
    ) => {
      encoded.push(text);
      return original(text, opts as Parameters<typeof QRCode.toString>[1]);
    }) as typeof QRCode.toString);
    try {
      const call = rpcStub({
        "device/list": () => ({ devices: [] }),
        "pair/create": () => ({ code: "ABCD-EFGH", expires_in: 300 }),
      });
      render(
        <SettingsRemoteTab call={call} initialRelayUrl="wss://relay.example.com/ws?session=abc" />,
      );
      fireEvent.click(await screen.findByRole("button", { name: "生成配对码" }));
      await screen.findByText("ABCD-EFGH");
      await waitFor(() => expect(encoded.length).toBeGreaterThan(0));

      expect(parsePairUri(encoded[encoded.length - 1])).toEqual({
        relay: "wss://relay.example.com/ws?session=abc",
        code: "ABCD-EFGH",
      });
    } finally {
      spy.mockRestore();
    }
  });

  it("omits the QR code when no relay url is entered", async () => {
    const call = rpcStub({
      "device/list": () => ({ devices: [] }),
      "pair/create": () => ({ code: "ABCD-EFGH", expires_in: 300 }),
    });
    render(<SettingsRemoteTab call={call} />);
    fireEvent.click(await screen.findByRole("button", { name: "生成配对码" }));
    await screen.findByText("ABCD-EFGH");
    expect(screen.queryByLabelText("配对二维码")).toBeNull();
  });
});
