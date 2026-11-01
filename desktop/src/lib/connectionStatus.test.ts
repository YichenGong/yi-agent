import { describe, expect, it } from "vitest";
import { connectionStatusLabel } from "./connectionStatus";

describe("connectionStatusLabel", () => {
  it("translates the states the transport reports", () => {
    expect(connectionStatusLabel("connected")).toBe("已连接");
    expect(connectionStatusLabel("connecting")).toBe("正在连接…");
    expect(connectionStatusLabel("exited")).toBe("已断开，正在重连…");
  });

  it("collapses the handshake failure to one actionable line", () => {
    // 握手失败时 App 把 `error: <message>` 塞进状态位。原文可能是英文的
    // `no computer connected for this session`，露在底栏既长又看不懂。
    expect(connectionStatusLabel("error: no computer connected for this session")).toBe(
      "连接失败，正在重试…",
    );
    expect(connectionStatusLabel("error: boom")).toBe("连接失败，正在重试…");
  });

  it("passes an unknown state through unchanged", () => {
    expect(connectionStatusLabel("something-else")).toBe("something-else");
  });
});
