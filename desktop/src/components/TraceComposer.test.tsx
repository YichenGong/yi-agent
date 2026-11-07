/** @vitest-environment jsdom */
/**
 * 常驻消息区。
 *
 * 这里守的几件事都不是「样式」而是**行为契约**：
 *
 * * 输入法：Enter 属于候选词选择而不是发送；而 WKWebView 在确认候选词时会把
 *   composition 先拆掉再送 keydown，所以 compositionend 之后还得靠 keyCode 229
 *   兜住，`onBlur` 也必须复位守卫——否则一次失焦就永久吞掉所有 Enter。
 * * 在途仍可编辑：发送中禁的是**再发一次**，不是让人不能打下一段字。
 * * 确认区替代动作行但不卸载 textarea：焦点与草稿都还在这儿。
 */
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { TraceComposer } from "./TraceComposer";
import type { TraceActionSnapshot } from "../lib/subagentTraceActions";

afterEach(cleanup);

function snapshot(overrides: Partial<TraceActionSnapshot> = {}): TraceActionSnapshot {
  return {
    draft: "",
    draftRevision: 0,
    messagePending: false,
    previewPending: false,
    confirmPending: false,
    confirmation: null,
    error: null,
    notice: null,
    ...overrides,
  };
}

function props(overrides: Partial<Parameters<typeof TraceComposer>[0]> = {}) {
  return {
    value: snapshot(),
    finished: false,
    onDraftChange: vi.fn(),
    onSend: vi.fn().mockResolvedValue(undefined),
    onPreview: vi.fn().mockResolvedValue(undefined),
    onConfirm: vi.fn().mockResolvedValue(undefined),
    onDismiss: vi.fn(),
    onFallbackFocus: vi.fn(),
    ...overrides,
  };
}

function textarea(): HTMLTextAreaElement {
  return screen.getByLabelText("向这个 agent 发送消息") as HTMLTextAreaElement;
}

function enter(node: HTMLElement, init: { shiftKey?: boolean; keyCode?: number; isComposing?: boolean } = {}) {
  fireEvent.keyDown(node, {
    key: "Enter",
    shiftKey: init.shiftKey ?? false,
    keyCode: init.keyCode ?? 13,
    isComposing: init.isComposing ?? false,
  });
}

describe("input", () => {
  it("labels the textarea independently of its placeholder", () => {
    render(<TraceComposer {...props()} />);
    expect(textarea().getAttribute("aria-label")).toBe("向这个 agent 发送消息");
    // 两行起步，高度上限四行，超出在控件内滚。
    expect(textarea().rows).toBe(2);
  });

  it("sends on Enter and leaves Shift+Enter to insert a newline", () => {
    const onSend = vi.fn().mockResolvedValue(undefined);
    render(<TraceComposer {...props({ value: snapshot({ draft: "hi" }), onSend })} />);

    enter(textarea(), { shiftKey: true });
    expect(onSend).not.toHaveBeenCalled();

    enter(textarea());
    expect(onSend).toHaveBeenCalledTimes(1);
  });

  it("does not send when the draft is blank", () => {
    const onSend = vi.fn().mockResolvedValue(undefined);
    render(<TraceComposer {...props({ value: snapshot({ draft: "   \n " }), onSend })} />);

    enter(textarea());
    // 按钮本身也禁用，两条路都堵住。
    expect(onSend).not.toHaveBeenCalled();
    expect((screen.getByRole("button", { name: "发送" }) as HTMLButtonElement).disabled).toBe(true);
  });

  it("does not send the Enter that confirms an IME candidate", () => {
    const onSend = vi.fn().mockResolvedValue(undefined);
    render(<TraceComposer {...props({ value: snapshot({ draft: "你好" }), onSend })} />);

    act(() => {
      fireEvent.compositionStart(textarea());
    });
    enter(textarea(), { isComposing: true });
    expect(onSend).not.toHaveBeenCalled();

    act(() => {
      fireEvent.compositionEnd(textarea());
    });
    // WKWebView 确认候选词的这一次 keydown 已经不带 isComposing 了。
    enter(textarea(), { keyCode: 229 });
    expect(onSend).not.toHaveBeenCalled();

    enter(textarea());
    expect(onSend).toHaveBeenCalledTimes(1);
  });

  it("recovers the IME guard when a composition is torn down by blur", () => {
    const onSend = vi.fn().mockResolvedValue(undefined);
    render(<TraceComposer {...props({ value: snapshot({ draft: "你好" }), onSend })} />);

    act(() => {
      fireEvent.compositionStart(textarea());
    });
    // 失焦可能不派发 compositionend：不复位守卫就会永久吞掉 Enter。
    act(() => {
      fireEvent.blur(textarea());
    });
    act(() => {
      textarea().focus();
    });

    enter(textarea());
    expect(onSend).toHaveBeenCalledTimes(1);
  });

  it("keeps the textarea editable while a message is in flight", () => {
    const onDraftChange = vi.fn();
    render(
      <TraceComposer
        {...props({ value: snapshot({ draft: "first", messagePending: true }), onDraftChange })}
      />,
    );

    // 在途禁的是再发一次，不是不能打下一段。
    expect(textarea().disabled).toBe(false);
    fireEvent.change(textarea(), { target: { value: "first and more" } });
    expect(onDraftChange).toHaveBeenCalledWith("first and more");
    expect((screen.getByRole("button", { name: "发送" }) as HTMLButtonElement).disabled).toBe(true);
  });

  it("does not fire two sends for a double Enter", async () => {
    let resolve: () => void = () => {};
    const onSend = vi.fn().mockReturnValue(
      new Promise<void>((done) => {
        resolve = done;
      }),
    );
    render(<TraceComposer {...props({ value: snapshot({ draft: "hi" }), onSend })} />);

    enter(textarea());
    enter(textarea());
    expect(onSend).toHaveBeenCalledTimes(1);

    await act(async () => {
      resolve();
    });
  });

  it("keeps the parent's draft when a send fails", async () => {
    const onSend = vi.fn().mockRejectedValue(new Error("no connection"));
    const onDraftChange = vi.fn();
    render(
      <TraceComposer
        {...props({ value: snapshot({ draft: "keep me" }), onSend, onDraftChange })}
      />,
    );

    await act(async () => {
      enter(textarea());
    });

    // 失败留稿：组件不替调用方清掉受控值。
    expect(onDraftChange).not.toHaveBeenCalledWith("");
    expect(textarea().value).toBe("keep me");
  });

  it("swallows the rejection it caused instead of leaking an unhandled one", async () => {
    const onSend = vi.fn().mockRejectedValue(new Error("no connection"));
    const onPreview = vi.fn().mockRejectedValue(new Error("no connection"));
    render(<TraceComposer {...props({ value: snapshot({ draft: "hi" }), onSend, onPreview })} />);

    await act(async () => {
      enter(textarea());
      fireEvent.click(screen.getByRole("button", { name: "停止任务" }));
    });
    // 走到这里不抛就是通过：错误由拥有者记录展示，这里只吞掉自己的 rejection。
    expect(onSend).toHaveBeenCalled();
    expect(onPreview).toHaveBeenCalled();
  });
});

describe("stop", () => {
  it("hides the stop control once the task is finished", () => {
    render(<TraceComposer {...props({ finished: true })} />);
    expect(screen.queryByRole("button", { name: "停止任务" })).toBeNull();
    // 消息区始终在：结束的任务仍可发消息。
    expect(textarea()).toBeTruthy();
  });

  it("asks before stopping and names the exact scope", () => {
    const { rerender } = render(<TraceComposer {...props()} />);
    rerender(
      <TraceComposer
        {...props({
          value: snapshot({ confirmation: { token: "t1", taskIds: ["task-a"], deadline: Date.now() + 60_000 } }),
        })}
      />,
    );
    expect(screen.getByText("停止该任务？")).toBeTruthy();

    rerender(
      <TraceComposer
        {...props({
          value: snapshot({
            confirmation: { token: "t1", taskIds: ["a", "b"], deadline: Date.now() + 60_000 },
          }),
        })}
      />,
    );
    // 只按服务端返回的范围说话，不暗示一定递归取消。
    expect(screen.getByText("将停止 2 个任务")).toBeTruthy();
  });

  it("replaces the action row while keeping the textarea mounted", () => {
    const { container } = render(
      <TraceComposer
        {...props({
          value: snapshot({
            draft: "still here",
            confirmation: { token: "t1", taskIds: ["task-a"], deadline: Date.now() + 60_000 },
          }),
        })}
      />,
    );

    // 确认区替代的是动作行，输入区不受影响。
    expect(screen.queryByRole("button", { name: "发送" })).toBeNull();
    expect(textarea().value).toBe("still here");
    expect(container.querySelector("textarea")).not.toBeNull();
  });

  it("disables dismissing while the confirm request is in flight", () => {
    render(
      <TraceComposer
        {...props({
          value: snapshot({
            confirmPending: true,
            confirmation: { token: "t1", taskIds: ["task-a"], deadline: Date.now() + 60_000 },
          }),
        })}
      />,
    );

    // 已发出的请求撤不回来，所以不能给一个「放弃」的假动作。
    expect((screen.getByRole("button", { name: "放弃" }) as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByRole("button", { name: "确认停止" }) as HTMLButtonElement).disabled).toBe(true);
  });

  it("disables confirming once the token has expired", () => {
    render(
      <TraceComposer
        {...props({
          value: snapshot({
            confirmation: { token: "t1", taskIds: ["task-a"], deadline: Date.now() - 1 },
          }),
        })}
      />,
    );

    expect((screen.getByRole("button", { name: "确认停止" }) as HTMLButtonElement).disabled).toBe(true);
    expect(screen.getByText("确认已过期，请重新预览")).toBeTruthy();
  });

  it("disables confirming when the scope is empty", () => {
    render(
      <TraceComposer
        {...props({
          value: snapshot({
            confirmation: { token: "t1", taskIds: [], deadline: Date.now() + 60_000 },
          }),
        })}
      />,
    );

    expect((screen.getByRole("button", { name: "确认停止" }) as HTMLButtonElement).disabled).toBe(true);
  });

  it("puts initial focus on the harmless choice when the confirmation opens", () => {
    const confirmation = { token: "t1", taskIds: ["task-a"], deadline: Date.now() + 60_000 };
    const { rerender } = render(<TraceComposer {...props()} />);

    // 真实路径是用户先点「停止任务」，焦点已经在那一栏里。
    screen.getByRole("button", { name: "停止任务" }).focus();

    // 键盘用户必须先落在「放弃」上：默认焦点若停在危险动作，一次 Enter 就停掉了任务。
    rerender(<TraceComposer {...props({ value: snapshot({ confirmation }) })} />);

    expect(document.activeElement).toBe(screen.getByRole("button", { name: "放弃" }));
  });

  it("does not pull focus into the confirmation when it opens from elsewhere", () => {
    const confirmation = { token: "t1", taskIds: ["task-a"], deadline: Date.now() + 60_000 };
    const field = document.createElement("input");
    document.body.append(field);
    field.focus();
    const { rerender } = render(<TraceComposer {...props()} />);

    rerender(<TraceComposer {...props({ value: snapshot({ confirmation }) })} />);

    expect(document.activeElement).toBe(field);
    field.remove();
  });

  it("returns focus to the stop control when the confirmation closes under it", () => {
    const onFallbackFocus = vi.fn();
    const confirmation = { token: "t1", taskIds: ["task-a"], deadline: Date.now() + 60_000 };
    const { rerender } = render(
      <TraceComposer {...props({ value: snapshot({ confirmation }), onFallbackFocus })} />,
    );

    const confirm = screen.getByRole("button", { name: "确认停止" });
    confirm.focus();

    // 确认区消失（成功/失败/过期都走这条），焦点不能留在被移除的节点上。
    rerender(<TraceComposer {...props({ onFallbackFocus })} />);

    const stop = screen.getByRole("button", { name: "停止任务" });
    expect(document.activeElement).toBe(stop);
  });

  it("falls back to the caller's target when the task ended and the stop control is gone", () => {
    const onFallbackFocus = vi.fn();
    const confirmation = { token: "t1", taskIds: ["task-a"], deadline: Date.now() + 60_000 };
    const { rerender } = render(
      <TraceComposer {...props({ value: snapshot({ confirmation }), onFallbackFocus })} />,
    );

    screen.getByRole("button", { name: "确认停止" }).focus();

    // 终态替换确认区：停止按钮也一起消失，只能回到标题。
    rerender(<TraceComposer {...props({ finished: true, onFallbackFocus })} />);

    expect(onFallbackFocus).toHaveBeenCalled();
  });

  it("keeps focus in the textarea after the user moved back to it", () => {
    const confirmation = { token: "t1", taskIds: ["task-a"], deadline: Date.now() + 60_000 };
    const { rerender } = render(<TraceComposer {...props()} />);

    screen.getByRole("button", { name: "停止任务" }).focus();
    rerender(<TraceComposer {...props({ value: snapshot({ confirmation }) })} />);
    expect(document.activeElement).toBe(screen.getByRole("button", { name: "放弃" }));

    // 用户改了主意，回去继续打字：确认区关门时不能再把人拽走。
    textarea().focus();
    rerender(<TraceComposer {...props()} />);

    expect(document.activeElement).toBe(textarea());
  });

  it("does not steal focus when the confirmation closes and focus is elsewhere", () => {
    const confirmation = { token: "t1", taskIds: ["task-a"], deadline: Date.now() + 60_000 };
    const { rerender } = render(<TraceComposer {...props({ value: snapshot({ confirmation }) })} />);

    const field = document.createElement("input");
    document.body.append(field);
    field.focus();

    rerender(<TraceComposer {...props()} />);

    // 用户正在别处输入，这一栏不能抢。
    expect(document.activeElement).toBe(field);
    field.remove();
  });
});

describe("feedback", () => {
  it("uses one polite status region and alerts for errors", () => {
    const { container } = render(
      <TraceComposer
        {...props({ value: snapshot({ notice: "消息已入队", error: "取消失败" }) })}
      />,
    );

    const statuses = container.querySelectorAll('[role="status"]');
    expect(statuses).toHaveLength(1);
    expect(statuses[0].getAttribute("aria-live")).toBe("polite");
    expect(screen.getByRole("alert").textContent).toContain("取消失败");
  });

  it("shows the waiting hint without pretending the outcome is known", () => {
    render(<TraceComposer {...props({ value: snapshot({ notice: "仍在等待服务端确认，结果尚未返回。" }) })} />);
    expect(screen.getByRole("status").textContent).toContain("结果尚未返回");
  });
});
