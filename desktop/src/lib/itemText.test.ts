import { describe, it, expect } from "vitest";
import { itemText } from "./itemText";
import type { Item } from "./protocol";

describe("itemText", () => {
  it("返回用户与助手消息的正文", () => {
    expect(itemText({ type: "userMessage", id: "u1", text: "hi" } as Item)).toBe("hi");
    expect(itemText({ type: "agentMessage", id: "a1", text: "yo" } as Item)).toBe("yo");
  });

  it("返回 reasoning 块的正文", () => {
    expect(itemText({ type: "reasoning", id: "r1", text: "thinking" })).toBe("thinking");
  });

  it("toolCall 没有可搜索正文，返回空串", () => {
    const tool = {
      type: "toolCall",
      id: "t1",
      call_id: "c1",
      name: "bash",
      input: { cmd: "ls" },
      status: "completed",
    } as unknown as Item;
    expect(itemText(tool)).toBe("");
  });

  it("notice 的正文走结构化载荷，返回空串", () => {
    expect(itemText({ type: "notice", id: "n1", text: "x" })).toBe("");
  });

  it("thinking 的明文（display omitted 时为空串）原样返回", () => {
    const t = { type: "thinking", id: "k1", text: "", redacted: true } as unknown as Item;
    expect(itemText(t)).toBe("");
  });
});
