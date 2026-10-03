import { describe, expect, it } from "vitest";
import { buildPairUri, parsePairUri, phoneRelayUrl } from "./pairUri";

// 与 Rust 侧 `pair_uri.rs` 的用例**逐字节**共用同一组 fixture。
describe("pairUri", () => {
  it("builds the canonical URI", () => {
    expect(buildPairUri("wss://relay.example.com/ws?session=abc", "ABCD-EFGH")).toBe(
      "yiagent://pair?v=1&relay=wss%3A%2F%2Frelay.example.com%2Fws%3Fsession%3Dabc&code=ABCD-EFGH",
    );
    expect(buildPairUri("ws://192.168.1.5:8080/ws", "WXYZ-1234")).toBe(
      "yiagent://pair?v=1&relay=ws%3A%2F%2F192.168.1.5%3A8080%2Fws&code=WXYZ-1234",
    );
    expect(buildPairUri("wss://r/a b.c?x=1&y=2", "A-B")).toBe(
      "yiagent://pair?v=1&relay=wss%3A%2F%2Fr%2Fa+b.c%3Fx%3D1%26y%3D2&code=A-B",
    );
  });

  it("round-trips", () => {
    for (const [relay, code] of [
      ["wss://relay.example.com/ws?session=abc", "ABCD-EFGH"],
      ["ws://192.168.1.5:8080/ws", "WXYZ-1234"],
    ]) {
      expect(parsePairUri(buildPairUri(relay, code))).toEqual({ relay, code });
    }
  });

  it("rejects malformed payloads", () => {
    expect(parsePairUri("hello")).toBeNull();
    expect(parsePairUri("https://pair?v=1&relay=wss%3A%2F%2Fr&code=C")).toBeNull();
    expect(parsePairUri("yiagent://pair?v=2&relay=wss%3A%2F%2Fr&code=C")).toBeNull();
    expect(parsePairUri("yiagent://pair?v=1&relay=wss%3A%2F%2Fr")).toBeNull();
    expect(parsePairUri("yiagent://pair?v=1&code=C")).toBeNull();
    expect(parsePairUri("yiagent://pair?v=1&relay=http%3A%2F%2Fr&code=C")).toBeNull();
    expect(parsePairUri("yiagent://pair?v=1&relay=&code=C")).toBeNull();
  });
});

// 手机侧中继端点由桌面侧端点派生：中继的两个出站路由是不同的（`/connect` 归电脑，
// `/ws` 归手机），把桌面地址原样塞进二维码会让手机注册成 agent。此处只换末段路径。
describe("phoneRelayUrl", () => {
  it("swaps only the trailing connect segment for ws", () => {
    expect(phoneRelayUrl("wss://relay.example.com/connect?session=x")).toBe(
      "wss://relay.example.com/ws?session=x",
    );
    // scheme/host/port/query 全保留
    expect(phoneRelayUrl("ws://192.168.1.5:8080/connect?session=abc&t=1")).toBe(
      "ws://192.168.1.5:8080/ws?session=abc&t=1",
    );
  });

  it("handles a path with extra segments by swapping only the last one", () => {
    expect(phoneRelayUrl("wss://relay.example.com/a/connect?session=x")).toBe(
      "wss://relay.example.com/a/ws?session=x",
    );
  });

  it("returns null for a path that is not connect (caller falls back)", () => {
    expect(phoneRelayUrl("wss://relay.example.com/ws?session=x")).toBeNull();
    expect(phoneRelayUrl("wss://relay.example.com/a/connect/x?session=x")).toBeNull();
    expect(phoneRelayUrl("wss://relay.example.com/connect/x")).toBeNull();
    expect(phoneRelayUrl("wss://relay.example.com/")).toBeNull();
  });

  it("returns null for unparseable input", () => {
    expect(phoneRelayUrl("not a url")).toBeNull();
    expect(phoneRelayUrl("")).toBeNull();
    expect(phoneRelayUrl("wss://")).toBeNull();
  });
});
