import { describe, expect, it } from "vitest";
import {
  parseSideCommand,
  sideInputPolicy,
  type SideInputPolicyInput,
} from "./sideCommands";

function base(over: Partial<SideInputPolicyInput> = {}): SideInputPolicyInput {
  return {
    platform: "desktop",
    view: "main",
    running: false,
    attachmentCount: 0,
    text: "",
    supported: true,
    canControl: true,
    ...over,
  };
}

describe("parseSideCommand", () => {
  it("parses /side with a question, trimming only the command boundary", () => {
    expect(parseSideCommand("/side why is this slow?")).toEqual({
      kind: "open",
      question: "why is this slow?",
    });
  });

  it("keeps inner whitespace of the question intact", () => {
    // 问题正文是用户要发给模型的内容，不是命令的一部分：只 trim 边界。
    expect(parseSideCommand("/btw   a  b  ")).toEqual({
      kind: "open",
      question: "a  b",
    });
  });

  it("allows an empty question", () => {
    expect(parseSideCommand("/side")).toEqual({ kind: "open", question: "" });
    expect(parseSideCommand("/btw   ")).toEqual({ kind: "open", question: "" });
  });

  it("treats a newline right after the command as an empty question", () => {
    expect(parseSideCommand("/side\nwhy?")).toEqual({
      kind: "open",
      question: "why?",
    });
  });

  it("does not treat /sidebar as /side", () => {
    expect(parseSideCommand("/sidebar")).toBeNull();
  });

  it("reports /back with arguments as invalid rather than ignoring them", () => {
    expect(parseSideCommand("/back now")).toEqual({
      kind: "invalid",
      message: "/back 不接受参数",
    });
  });

  it("reports /side-close with arguments as invalid", () => {
    expect(parseSideCommand("/side-close please")).toEqual({
      kind: "invalid",
      message: "/side-close 不接受参数",
    });
  });

  it("parses bare /back and /side-close", () => {
    expect(parseSideCommand("/back")).toEqual({ kind: "back" });
    expect(parseSideCommand("/side-close")).toEqual({ kind: "close" });
  });

  it("leaves ordinary text and absolute paths alone", () => {
    expect(parseSideCommand("why is it slow")).toBeNull();
    expect(parseSideCommand("/Users/me/src")).toBeNull();
  });

  it("does not confuse /btw with a close command", () => {
    expect(parseSideCommand("/btw what about tests")).toEqual({
      kind: "open",
      question: "what about tests",
    });
  });
});

describe("sideInputPolicy: main view", () => {
  it("sends ordinary text to the parent when idle", () => {
    expect(sideInputPolicy(base({ text: "hello" }))).toEqual({ action: "sendMain" });
  });

  it("sends ordinary text to the parent on desktop while running", () => {
    // 桌面的运行中插话是既有能力，旁支不改它。
    expect(
      sideInputPolicy(base({ platform: "desktop", running: true, text: "hello" })),
    ).toEqual({ action: "sendMain" });
  });

  it("keeps mobile main messages stop-only but permits exact side commands", () => {
    const mobile = base({ platform: "mobile", view: "main", running: true });
    expect(sideInputPolicy({ ...mobile, text: "ordinary" }).action).toBe("stop");
    expect(sideInputPolicy({ ...mobile, text: "/btw question" }).action).toBe(
      "executeSideCommand",
    );
    expect(sideInputPolicy({ ...mobile, text: "/side question" }).action).toBe(
      "executeSideCommand",
    );
  });

  it("keeps other commands stop-only on mobile while running", () => {
    const mobile = base({ platform: "mobile", view: "main", running: true });
    expect(sideInputPolicy({ ...mobile, text: "/sidebar" }).action).toBe("stop");
    expect(sideInputPolicy({ ...mobile, text: "/help" }).action).toBe("stop");
    expect(sideInputPolicy({ ...mobile, text: "/clear" }).action).toBe("stop");
  });

  it("allows commands when the mobile parent is idle", () => {
    expect(
      sideInputPolicy(base({ platform: "mobile", text: "/btw q" })).action,
    ).toBe("executeSideCommand");
    expect(sideInputPolicy(base({ platform: "mobile", text: "hello" })).action).toBe(
      "sendMain",
    );
  });

  it("blocks any command that carries pending attachments", () => {
    for (const text of ["/btw q", "/side", "/back", "/side-close", "/help"]) {
      const decision = sideInputPolicy(base({ text, attachmentCount: 1 }));
      expect(decision.action).toBe("blocked");
      expect(decision.reason).toContain("附件");
    }
  });

  it("does not block ordinary text with attachments", () => {
    expect(sideInputPolicy(base({ text: "look at this", attachmentCount: 2 })).action).toBe(
      "sendMain",
    );
  });
});

describe("sideInputPolicy: opening a side", () => {
  it("opens a side from an idle parent", () => {
    expect(sideInputPolicy(base({ text: "/btw why" }))).toEqual({
      action: "executeSideCommand",
      command: { kind: "open", question: "why" },
    });
  });

  it("opens a side from a running parent without interrupting it", () => {
    const decision = sideInputPolicy(base({ running: true, text: "/btw why" }));
    expect(decision.action).toBe("executeSideCommand");
  });

  it("refuses to open when the host lacks the capability", () => {
    const decision = sideInputPolicy(base({ supported: false, text: "/side" }));
    expect(decision.action).toBe("blocked");
    expect(decision.reason).toContain("不支持");
  });

  it("lets a read-only device view an existing side with a bare /side", () => {
    // 无参 `/side` 在 Observe 上是「查看已有旁问」：只 list + readItems，
    // 不创建 runner、不提问，所以不该被权限挡住（spec §4.3）。
    expect(
      sideInputPolicy(base({ canControl: false, text: "/side" })),
    ).toEqual({ action: "executeSideCommand", command: { kind: "open", question: "" } });
    expect(
      sideInputPolicy(base({ canControl: false, text: "/btw  " })).action,
    ).toBe("executeSideCommand");
  });

  it("refuses to ask a question from a read-only device", () => {
    const decision = sideInputPolicy(
      base({ canControl: false, text: "/btw why is it slow?" }),
    );
    expect(decision.action).toBe("blocked");
    expect(decision.reason).toContain("只能查看");
  });

  it("refuses a question from a read-only device even without extra words", () => {
    const decision = sideInputPolicy(
      base({ canControl: false, text: "/side ping" }),
    );
    expect(decision.action).toBe("blocked");
    expect(decision.reason).toContain("只读");
  });

  it("refuses nested sides instead of re-routing to the parent input", () => {
    const decision = sideInputPolicy(base({ view: "side", text: "/btw again" }));
    expect(decision.action).toBe("blocked");
    expect(decision.reason).toContain("再次发起");
  });
});

describe("sideInputPolicy: side view", () => {
  it("sends text to the side when the side is idle", () => {
    expect(sideInputPolicy(base({ view: "side", text: "follow up" }))).toEqual({
      action: "sendSide",
    });
  });

  it("stops the side (not the parent) while the side runs", () => {
    expect(
      sideInputPolicy(base({ view: "side", running: true, text: "follow up" })),
    ).toEqual({ action: "stop" });
  });

  it("keeps a running side stop-only even on desktop", () => {
    // 旁支的「运行中」是它自己的状态，与父的插话能力无关。
    expect(
      sideInputPolicy(
        base({ platform: "desktop", view: "side", running: true, text: "x" }),
      ).action,
    ).toBe("stop");
  });

  it("executes /back inside a side", () => {
    expect(sideInputPolicy(base({ view: "side", text: "/back" }))).toEqual({
      action: "executeSideCommand",
      command: { kind: "back" },
    });
  });

  it("executes /side-close inside a side", () => {
    expect(sideInputPolicy(base({ view: "side", text: "/side-close" }))).toEqual({
      action: "executeSideCommand",
      command: { kind: "close" },
    });
  });

  it("allows the status commands inside a side", () => {
    for (const name of ["help", "config", "usage"]) {
      const decision = sideInputPolicy(base({ view: "side", text: `/${name}` }));
      expect(decision.action).toBe("executeSideCommand");
      expect(decision.command).toMatchObject({ kind: "command", name });
    }
  });

  it("refuses clear/compact/fork inside a side and keeps the input", () => {
    for (const name of ["clear", "compact", "fork"]) {
      const decision = sideInputPolicy(base({ view: "side", text: `/${name}` }));
      expect(decision.action).toBe("blocked");
      expect(decision.reason).toContain(`/${name}`);
    }
  });

  it("refuses /side-close on a read-only device", () => {
    const decision = sideInputPolicy(
      base({ view: "side", canControl: false, text: "/side-close" }),
    );
    expect(decision.action).toBe("blocked");
  });

  it("allows /side-close from the main view for a Control device", () => {
    // 主视图也能关掉后台旁问：旁支只在视图里显示，不该成为「必须先进入」的
    // 前提。没有旁问时由执行方给幂等提示，而不是在这里预先拒绝。
    expect(
      sideInputPolicy(base({ view: "main", text: "/side-close" })),
    ).toEqual({ action: "executeSideCommand", command: { kind: "close" } });
  });

  it("refuses /side-close from the main view on a read-only device", () => {
    expect(
      sideInputPolicy(base({ view: "main", canControl: false, text: "/side-close" }))
        .action,
    ).toBe("blocked");
  });

  it("still allows /back from the main view as a no-op command", () => {
    // 返回不改变任何服务端状态，主视图里执行它是无害的空操作，由调用方提示。
    expect(sideInputPolicy(base({ view: "main", text: "/back" })).action).toBe(
      "executeSideCommand",
    );
  });
});

describe("sideInputPolicy: aliases and lookalikes", () => {
  it("is not fooled by a command prefix", () => {
    expect(sideInputPolicy(base({ text: "/sides" })).action).toBe("sendMain");
    expect(
      sideInputPolicy(base({ platform: "mobile", running: true, text: "/sides" })).action,
    ).toBe("stop");
  });

  it("treats a path that starts with the command as an ordinary message", () => {
    expect(sideInputPolicy(base({ text: "/side/foo" })).action).toBe("sendMain");
  });

  it("does not execute a command twice for tab completion", () => {
    // Tab 只填候选（`/side `），不提交；填入后仍是一条待执行的命令。
    const filled = sideInputPolicy(base({ text: "/side " }));
    expect(filled.action).toBe("executeSideCommand");
    expect(filled.command).toEqual({ kind: "open", question: "" });
  });
});
