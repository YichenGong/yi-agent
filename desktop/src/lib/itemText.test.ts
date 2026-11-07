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

  it("普通文本 notice 渲染的就是 text，因此必须可搜", () => {
    // `ChatView` 的 notice 分支对缺省 `body` 直接渲染 `{item.text}`（琥珀色胶囊）。
    // 若这里返回空串，用户在界面上看得见的提示却搜不到——搜索与渲染分叉。
    expect(itemText({ type: "notice", id: "n1", text: "通道回退到 backup" })).toBe(
      "通道回退到 backup",
    );
  });

  it("结构化 notice 渲染成卡片，正文不是 text，返回空串", () => {
    const n = {
      type: "notice",
      id: "n2",
      text: "/config",
      body: { kind: "config", data: { model: "m" } },
    } as unknown as Item;
    expect(itemText(n)).toBe("");
  });

  it("thinking 有明文时返回明文（不能用空串用例代替）", () => {
    // 空串用例与 `default: return ""` 的结果无法区分，钉不住 `thinking` 落在
    // `return item.text` 分组里这个事实；而 CoT 就地追加能否被比较器看见，
    // 正取决于此。所以必须用非空明文。
    const t = { type: "thinking", id: "k2", text: "真实摘要", redacted: false } as unknown as Item;
    expect(itemText(t)).toBe("真实摘要");
  });

  it("thinking 的明文（display omitted 时为空串）原样返回", () => {
    const t = { type: "thinking", id: "k1", text: "", redacted: true } as unknown as Item;
    expect(itemText(t)).toBe("");
  });
});
