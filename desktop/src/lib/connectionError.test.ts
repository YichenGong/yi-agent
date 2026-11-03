import { describe, expect, it } from "vitest";
import {
  CONNECTION_DISCONNECTED_TEXT,
  ConnectionError,
  NO_COMPUTER_TEXT,
  clearConnectionErrorFrom,
  connectionErrorKind,
  isConnectionMessage,
} from "./connectionError";
import { formatError } from "./errorMessage";

describe("connectionErrorKind", () => {
  it("recognizes our own structured error", () => {
    expect(connectionErrorKind(new ConnectionError("disconnected"))).toBe("disconnected");
    expect(connectionErrorKind(new ConnectionError("no_computer"))).toBe("no_computer");
  });

  it("recognizes the relays no-agent frame by its code", () => {
    // 中继在没有电脑时回的最小错误帧（yi-agent-relay/src/lib.rs:175）。
    // 它没有 id、没有 method，`RpcClient` 把它当作在途请求的失败原因抛出。
    expect(
      connectionErrorKind({ code: -32000, message: "no computer connected for this session" }),
    ).toBe("no_computer");
  });

  it("recognizes the transport/seam failure messages", () => {
    expect(connectionErrorKind(new Error("rpc client disposed"))).toBe("disconnected");
    expect(connectionErrorKind(new Error("websocket closed (1006)"))).toBe("disconnected");
    expect(connectionErrorKind(new Error("websocket closed (unknown)"))).toBe("disconnected");
    expect(connectionErrorKind(new Error("not connected"))).toBe("disconnected");
  });

  it("leaves business errors alone", () => {
    expect(connectionErrorKind(new Error("unknown thread"))).toBeNull();
    expect(connectionErrorKind({ code: -32011, message: "unknown thread" })).toBeNull();
    expect(connectionErrorKind(null)).toBeNull();
    expect(connectionErrorKind("plain string")).toBeNull();
  });
});

describe("formatError on connection failures", () => {
  it("renders actionable Chinese copy instead of the raw English", () => {
    expect(formatError(new Error("rpc client disposed"))).toBe(CONNECTION_DISCONNECTED_TEXT);
    expect(formatError(new Error("websocket closed (1006)"))).toBe(CONNECTION_DISCONNECTED_TEXT);
    expect(formatError(new Error("not connected"))).toBe(CONNECTION_DISCONNECTED_TEXT);
    expect(
      formatError({ code: -32000, message: "no computer connected for this session" }),
    ).toBe(NO_COMPUTER_TEXT);
  });

  it("keeps translating unrelated errors the old way", () => {
    expect(formatError({ code: -32011, message: "unknown thread" })).toBe("unknown thread");
    expect(formatError(new Error("boom"))).toBe("boom");
  });
});

describe("isConnectionMessage / clearConnectionErrorFrom", () => {
  it("recognizes both rendered copies", () => {
    expect(isConnectionMessage(CONNECTION_DISCONNECTED_TEXT)).toBe(true);
    expect(isConnectionMessage(NO_COMPUTER_TEXT)).toBe(true);
    expect(isConnectionMessage("unknown thread")).toBe(false);
  });

  it("clears only a connection notice, reporting whether it changed", () => {
    const session = { lastError: CONNECTION_DISCONNECTED_TEXT as string | null };
    expect(clearConnectionErrorFrom(session)).toBe(true);
    expect(session.lastError).toBeNull();

    // 已清空 / 业务错误都不动，返回 false。
    expect(clearConnectionErrorFrom(session)).toBe(false);
    const business = { lastError: "unknown thread" as string | null };
    expect(clearConnectionErrorFrom(business)).toBe(false);
    expect(business.lastError).toBe("unknown thread");
  });
});
