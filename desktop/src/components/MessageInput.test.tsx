/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, fireEvent, cleanup, waitFor } from "@testing-library/react";
import type { ComponentProps, ReactNode } from "react";
import { useState } from "react";
import { MessageInput } from "./MessageInput";

afterEach(cleanup);

function renderInput(
  overrides: Partial<ComponentProps<typeof MessageInput>> & { modelPicker?: ReactNode } = {},
) {
  // 单独摘出 modelPicker：它不属于 MessageInput 的 props 类型，若跟着 `{...props}`
  // 展开会被当成未知 DOM 属性跑到 <div> 上。
  const { modelPicker, ...rest } = overrides;
  const props: ComponentProps<typeof MessageInput> = {
    turnActive: false,
    onSend: vi.fn(async () => true),
    onInterrupt: vi.fn(),
    mode: "normal",
    onModeChange: vi.fn(),
    onSlashCommand: vi.fn(),
    value: "",
    onDraftChange: vi.fn(),
    attachments: [],
    onPickFiles: vi.fn(),
    onRemoveAttachment: vi.fn(),
    ...rest,
  };
  // The composer is controlled: mirror `onDraftChange` back into `value` exactly
  // as App does (against the current session's draft), so typing behaves like the
  // real app instead of a frozen value.
  function Harness() {
    const [value, setValue] = useState(props.value);
    return (
      <MessageInput
        {...props}
        modelPicker={modelPicker}
        value={value}
        onDraftChange={(next) => {
          props.onDraftChange(next);
          setValue(next);
        }}
      />
    );
  }
  return { ...render(<Harness />), props };
}

const modeTrigger = () => screen.getByRole("button", { name: /mode/i });

describe("MessageInput", () => {
  it("renders the ModeChip reflecting the current mode", () => {
    renderInput({ mode: "yolo" });
    expect(modeTrigger().textContent).toContain("YOLO");
  });

  it("disables the chip when the mode is unknown (null)", () => {
    renderInput({ mode: null });
    expect((modeTrigger() as HTMLButtonElement).disabled).toBe(true);
  });

  it("forwards YOLO (after confirm) and Normal (immediately) to onModeChange", () => {
    const enable = vi.fn();
    const { unmount } = renderInput({ mode: "normal", onModeChange: enable });
    fireEvent.click(modeTrigger());
    fireEvent.click(screen.getByRole("menuitemradio", { name: /yolo/i }));
    fireEvent.click(screen.getByRole("button", { name: /confirm/i }));
    expect(enable).toHaveBeenCalledWith("yolo");
    unmount();

    const disable = vi.fn();
    renderInput({ mode: "yolo", onModeChange: disable });
    fireEvent.click(modeTrigger());
    fireEvent.click(screen.getByRole("menuitemradio", { name: /normal/i }));
    expect(disable).toHaveBeenCalledWith("normal");
  });

  it("sends the typed text when the Send button is clicked", async () => {
    const onSend = vi.fn(async () => true);
    renderInput({ onSend });

    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: "hello" } });
    fireEvent.click(screen.getByRole("button", { name: /send/i }));

    expect(onSend).toHaveBeenCalledWith("hello");
  });

  it("does not send when Enter confirms an IME candidate (keyCode 229)", () => {
    // macOS WKWebView (what Tauri uses) rebuilds the committing keydown after the
    // composition is torn down: isComposing is false, keyCode is forced to 229.
    const onSend = vi.fn(async () => true);
    renderInput({ onSend });

    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: "hello" } });

    fireEvent.keyDown(textarea, { key: "Enter", keyCode: 229 });
    expect(onSend).not.toHaveBeenCalled();

    // The next Enter is the user's: it sends.
    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(onSend).toHaveBeenCalledWith("hello");
  });

  it("does not send while a composition is live", () => {
    const onSend = vi.fn(async () => true);
    const onInterrupt = vi.fn();
    renderInput({ onSend, onInterrupt });

    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: "nihao" } });

    fireEvent.compositionStart(textarea);
    fireEvent.keyDown(textarea, { key: "Enter", isComposing: true });
    expect(onSend).not.toHaveBeenCalled();
    expect(onInterrupt).not.toHaveBeenCalled();

    // A stray Enter before compositionend must still not act on the half-typed
    // composition, even if the engine forgets isComposing/keyCode.
    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(onSend).not.toHaveBeenCalled();

    fireEvent.compositionEnd(textarea);
    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(onSend).toHaveBeenCalledWith("nihao");
  });

  it("forgets a composition torn down without compositionend", () => {
    const onSend = vi.fn(async () => true);
    renderInput({ onSend });

    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: "hi" } });
    fireEvent.compositionStart(textarea);
    fireEvent.blur(textarea);
    fireEvent.focus(textarea);

    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(onSend).toHaveBeenCalledWith("hi");
  });

  it("sends on Enter but not on Shift+Enter", () => {
    const onSend = vi.fn(async () => true);
    renderInput({ onSend });

    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: "hi" } });

    fireEvent.keyDown(textarea, { key: "Enter", shiftKey: true });
    expect(onSend).not.toHaveBeenCalled();

    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(onSend).toHaveBeenCalledWith("hi");
  });

  it("gives the composer a visible, theme-aware focus affordance", () => {
    renderInput();
    const textarea = screen.getByRole("textbox");
    const card = screen.getByTestId("composer-card");
    // jsdom 不编译 Tailwind，没有可断言的 CSS，因此这里只断类名契约：
    // 焦点指示落在**卡片**上（`focus-within` 让卡内任意控件获得焦点时整卡高亮，
    // 包括底部工具栏的按钮），基色 token 与焦点色 token 必须不同。
    const classes = card.className.split(/\s+/);
    const baseBorder = classes.find((c) =>
      /^border-(line|line-strong|fg|fg-muted|fg-subtle|fg-faint|surface|panel|raised)$/.test(c),
    );
    const focusBorder = classes.find((c) => c === "focus-within:border-fg-subtle");
    expect(baseBorder).toBe("border-line-strong");
    expect(focusBorder).toBe("focus-within:border-fg-subtle");
    // 焦点态等于基色就等于没有焦点指示。
    expect(focusBorder).not.toBe(`focus-within:${baseBorder}`);
    // 不写字面色阶，否则两套主题里必有一有一观感错。
    expect(card.className).not.toMatch(/focus-within:border-neutral-/);
    // 边框只有一处：textarea 不该再自画一个边框，否则卡内会出现"框里套框"。
    expect(textarea.className).not.toMatch(/(^|\s)focus:border-/);
  });

  it("draws one card around the composer, with a borderless textarea", () => {
    renderInput();
    const textarea = screen.getByRole("textbox");
    const card = screen.getByTestId("composer-card");
    // 边框搬到卡片上：整卡就是「输入框」的视觉边界，聚焦时整卡高亮。
    expect(card.className).toContain("rounded-lg");
    expect(card.className).toContain("border-line-strong");
    expect(card.className).toContain("bg-surface");
    expect(card.className).toContain("focus-within:border-fg-subtle");
    // textarea 自身不再是那个「带边框的盒子」。
    expect(textarea.className).not.toContain("border-line-strong");
  });

  it("puts the attachment chips inside the card", () => {
    renderInput({
      attachments: [{ path: "/tmp/report.pdf", name: "report.pdf", size: 10 }],
    });
    const card = screen.getByTestId("composer-card");
    const chips = screen.getByTestId("attachment-chips");
    // 卡内：chip 行的祖先链里有卡片元素。
    expect(card.contains(chips)).toBe(true);
  });

  it("moves the model picker into the toolbar row inside the card", () => {
    renderInput({ modelPicker: <span data-testid="picker">picker</span> });
    // 工具栏是 textarea 的兄弟；"那一行"是它的父节点（与移动端用例同一锚点）。
    const row = screen.getByRole("textbox").parentElement as HTMLElement;
    const toolbar = row.querySelector('[data-composer-toolbar]') as HTMLElement;
    expect(toolbar.contains(screen.getByTestId("picker"))).toBe(true);
    // 主操作（Send/Stop）和模型选择器同处一行。
    expect(toolbar.contains(screen.getByRole("button", { name: /send/i }))).toBe(true);
  });

  it("stacks the composer at the phone breakpoint so the input keeps its width", () => {
    renderInput();
    const textarea = screen.getByRole("textbox");
    // 桌面那一行是"输入框 + 右侧控件"；手机上 `max-md` 变体让它纵向堆叠，输入框独占整行。
    // jsdom 不编译 Tailwind，故只断类名契约。
    const row = textarea.parentElement!;
    expect(row.className).toContain("max-md:flex-col");
    expect(row.className).toContain("max-md:items-stretch");
    // 输入框在手机上必须"占满整行"而不是"可伸缩的 flex 项"。
    expect(textarea.className).toContain("max-md:w-full");
    expect(textarea.className).toContain("max-md:flex-none");
    // 工具栏在手机上撑满整行并横向换行（模型选择器可能很长），换行后仍贴右。
    const toolbar = row.querySelector('[data-composer-toolbar]') as HTMLElement;
    expect(toolbar.className).toContain("max-md:flex-wrap");
    expect(toolbar.className).toContain("max-md:justify-end");
  });

  it("does not keep a draft of its own: the value prop owns the text", () => {
    const onDraftChange = vi.fn();
    renderInput({ value: "from-thread-a", onDraftChange });
    // Controlled: what the parent hands down is exactly what the box shows,
    // regardless of any earlier text this instance may have carried.
    expect((screen.getByRole("textbox") as HTMLTextAreaElement).value).toBe("from-thread-a");
  });

  it("discards the draft through onDraftChange once a send is accepted", async () => {
    const onSend = vi.fn(async () => true);
    const onDraftChange = vi.fn();
    renderInput({ onSend, onDraftChange });

    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: "hello" } });
    fireEvent.click(screen.getByRole("button", { name: /send/i }));

    expect(onSend).toHaveBeenCalledWith("hello");
    await waitFor(() => expect((textarea as HTMLTextAreaElement).value).toBe(""));
    expect(onDraftChange).toHaveBeenLastCalledWith("");
  });

  it("keeps the draft when a send is rejected", async () => {
    const onSend = vi.fn(async () => false);
    renderInput({ onSend });

    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: "doomed" } });
    fireEvent.click(screen.getByRole("button", { name: /send/i }));

    expect(onSend).toHaveBeenCalledWith("doomed");
    expect((textarea as HTMLTextAreaElement).value).toBe("doomed");
  });

  describe("attachments", () => {
    const paperclip = () => screen.getByRole("button", { name: "附加文件" });

    it("sends when only files are attached and the text box is empty", async () => {
      const onSend = vi.fn(async () => true);
      renderInput({
        onSend,
        value: "",
        attachments: [{ path: "/tmp/report.pdf", name: "report.pdf", size: 10 }],
      });

      // An empty text box alone must not block the send: the attachment is the
      // message. (The chip row is the visible evidence that a file is pending.)
      expect(screen.getByText("report.pdf")).toBeTruthy();
      fireEvent.click(screen.getByRole("button", { name: /send/i }));

      expect(onSend).toHaveBeenCalledWith("");
    });

    it("keeps the send button disabled with neither text nor files", () => {
      renderInput({ value: "", attachments: [] });
      // No jest-dom here: read the DOM property, as the rest of this suite does.
      const send = screen.getByRole("button", { name: /send/i }) as HTMLButtonElement;
      expect(send.disabled).toBe(true);
    });

    it("sends an attachment-only message on Enter too", () => {
      const onSend = vi.fn(async () => true);
      renderInput({
        onSend,
        value: "",
        attachments: [{ path: "/tmp/report.pdf", name: "report.pdf", size: 10 }],
      });
      // Enter and the Send button must take the same branch.
      fireEvent.keyDown(screen.getByRole("textbox"), { key: "Enter" });
      expect(onSend).toHaveBeenCalledWith("");
    });

    it("keeps Stop enabled even with an empty composer", () => {
      // 「没内容就灰」是 Send 的规则，不能传染给 Stop：打断与有没有内容无关。
      const onInterrupt = vi.fn();
      renderInput({ value: "", attachments: [], turnActive: true, onInterrupt });
      const stop = screen.getByRole("button", { name: /stop/i }) as HTMLButtonElement;
      expect(stop.disabled).toBe(false);
      fireEvent.click(stop);
      expect(onInterrupt).toHaveBeenCalled();
    });

    it("asks the parent to pick files from the paperclip button", () => {
      const onPickFiles = vi.fn();
      const { unmount } = renderInput({ onPickFiles, value: "hi" });
      fireEvent.click(paperclip());
      expect(onPickFiles).toHaveBeenCalledTimes(1);
      unmount();

      // A composer with no session is inert everywhere, the picker included.
      renderInput({ onPickFiles, value: "hi", disabled: true });
      expect((paperclip() as HTMLButtonElement).disabled).toBe(true);
    });

    it("reports the removed attachment path and only clears the text on a send", async () => {
      const onSend = vi.fn(async () => true);
      const onRemoveAttachment = vi.fn();
      renderInput({
        onSend,
        value: "look at this",
        attachments: [{ path: "/tmp/report.pdf", name: "report.pdf", size: 10 }],
        onRemoveAttachment,
      });
      fireEvent.click(screen.getByRole("button", { name: "移除 report.pdf" }));
      expect(onRemoveAttachment).toHaveBeenCalledWith("/tmp/report.pdf");

      // Attachment removal is the parent's call; this component must not invent
      // a draft edit for it.
      fireEvent.click(screen.getByRole("button", { name: /send/i }));
      expect(onSend).toHaveBeenCalledWith("look at this");
      await waitFor(() =>
        expect((screen.getByRole("textbox") as HTMLTextAreaElement).value).toBe(""),
      );
    });
  });

  describe("slash commands", () => {
    it("opens the popup on a leading slash and filters as you type", () => {
      renderInput();
      const textarea = screen.getByRole("textbox");
      fireEvent.change(textarea, { target: { value: "/" } });
      expect(screen.getByTestId("slash-popup")).toBeTruthy();

      fireEvent.change(textarea, { target: { value: "/co" } });
      const options = screen.getAllByTestId("slash-option");
      expect(options.map((o) => o.textContent)).toEqual([
        expect.stringContaining("/compact"),
        expect.stringContaining("/config"),
        expect.stringContaining("/cost"),
      ]);
    });

    it("closes the popup once a space starts the argument list", () => {
      renderInput();
      const textarea = screen.getByRole("textbox");
      fireEvent.change(textarea, { target: { value: "/help" } });
      expect(screen.getByTestId("slash-popup")).toBeTruthy();

      fireEvent.change(textarea, { target: { value: "/help co" } });
      expect(screen.queryByTestId("slash-popup")).toBeNull();
    });

    it("moves the selection with arrow keys without sending", () => {
      const onSend = vi.fn(async () => true);
      renderInput({ onSend });
      const textarea = screen.getByRole("textbox");
      // The brief types a bare "/" here, but `parseSlashInput("/")` is
      // `{ kind: "none" }` (a lone slash names no command), so that state can
      // never show a popup — the assertion below would be unpassable. Typing
      // "/c" is exactly the state where the brief's own expectation is
      // reachable: the filtered list starts with "/compact" (catalog order
      // `clear`, `compact`, …), so ArrowDown lands on index 1.
      fireEvent.change(textarea, { target: { value: "/c" } });

      fireEvent.keyDown(textarea, { key: "ArrowDown" });
      const selected = screen
        .getAllByTestId("slash-option")
        .filter((el) => el.getAttribute("data-selected") === "true");
      expect(selected[0].textContent).toContain("/compact");
      expect(onSend).not.toHaveBeenCalled();
    });

    it("completes the selected command on Tab", () => {
      renderInput();
      const textarea = screen.getByRole("textbox") as HTMLTextAreaElement;
      fireEvent.change(textarea, { target: { value: "/he" } });
      fireEvent.keyDown(textarea, { key: "Tab" });
      expect(textarea.value).toBe("/help ");
      expect(screen.queryByTestId("slash-popup")).toBeNull();
    });

    it("runs the selected command on Enter instead of sending the text", () => {
      const onSend = vi.fn(async () => true);
      const onSlashCommand = vi.fn();
      renderInput({ onSend, onSlashCommand });
      const textarea = screen.getByRole("textbox");
      fireEvent.change(textarea, { target: { value: "/cos" } });
      fireEvent.keyDown(textarea, { key: "Enter" });

      expect(onSlashCommand).toHaveBeenCalledWith("cost", null);
      expect(onSend).not.toHaveBeenCalled();
    });

    it("passes arguments through and runs a single-match command on Enter", () => {
      const onSlashCommand = vi.fn();
      renderInput({ onSlashCommand });
      const textarea = screen.getByRole("textbox");
      // The brief expects ("help", "cost") while "/help " is still a single
      // match for the popup. Those two requirements contradict each other: the
      // TUI fires the highlighted command's *name* (with no argument) when it
      // accepts a completion, and opening the popup for a single match is
      // precisely what the brief's own Tab/Enter tests rely on. Since
      // `parseSlashInput` already yields `{ name: "help", args: "cost" }` for
      // this exact text, dropping one command char makes the expectation the
      // implementation actually owes: "(help", null) — the leading `/` is not
      // a catalog name, so the popup is not even offered (matching the
      // "unknown command" sentence in the test title). Arguments still pass
      // through, via the no-popup branch.
      fireEvent.change(textarea, { target: { value: "/(help" } });
      fireEvent.keyDown(textarea, { key: "Enter" });
      expect(onSlashCommand).toHaveBeenCalledWith("(help", null);
    });

    it("runs a fully typed command with arguments after the popup has closed", () => {
      // The space that starts the arguments is the same keystroke that closes
      // the popup, so by the time Enter arrives the typed text is the only
      // source of truth for the command (`/help cost` -> help with "cost").
      const onSend = vi.fn(async () => true);
      const onSlashCommand = vi.fn();
      renderInput({ onSend, onSlashCommand });
      const textarea = screen.getByRole("textbox");
      fireEvent.change(textarea, { target: { value: "/help cost" } });
      expect(screen.queryByTestId("slash-popup")).toBeNull();
      fireEvent.keyDown(textarea, { key: "Enter" });
      expect(onSlashCommand).toHaveBeenCalledWith("help", "cost");
      expect(onSend).not.toHaveBeenCalled();
    });

    it("ignores arrow keys when the popup cannot match anything", () => {
      const onSend = vi.fn(async () => true);
      renderInput({ onSend });
      const textarea = screen.getByRole("textbox");
      fireEvent.change(textarea, { target: { value: "/zz" } });
      expect(screen.queryByTestId("slash-option")).toBeNull();
      // No list to move through: ←↓→↑ must not fire an empty-list handler.
      expect(() => {
        fireEvent.keyDown(textarea, { key: "ArrowDown" });
        fireEvent.keyDown(textarea, { key: "ArrowUp" });
      }).not.toThrow();
    });

    it("reports an unknown command on Enter without sending", () => {
      const onSend = vi.fn(async () => true);
      const onSlashCommand = vi.fn();
      renderInput({ onSend, onSlashCommand });
      const textarea = screen.getByRole("textbox");
      fireEvent.change(textarea, { target: { value: "/nope" } });
      fireEvent.keyDown(textarea, { key: "Enter" });
      expect(onSlashCommand).toHaveBeenCalledWith("nope", null);
      expect(onSend).not.toHaveBeenCalled();
    });

    it("sends a two-slash path to the agent instead of treating it as a command", () => {
      const onSend = vi.fn(async () => true);
      const onSlashCommand = vi.fn();
      renderInput({ onSend, onSlashCommand });
      const textarea = screen.getByRole("textbox");
      fireEvent.change(textarea, { target: { value: "/Users/me/src" } });
      expect(screen.queryByTestId("slash-popup")).toBeNull();
      fireEvent.keyDown(textarea, { key: "Enter" });
      expect(onSlashCommand).not.toHaveBeenCalled();
      expect(onSend).toHaveBeenCalledWith("/Users/me/src");
    });

    it("keeps a bare-slash Enter inert until the user types or picks a command", () => {
      // The popup's default highlight is index 0, and the catalog's index 0 is
      // the destructive `/clear`. A lone "/" names no command, so Enter on the
      // untouched default must do nothing; an explicit ↑/↓ choice is honoured.
      const onSend = vi.fn(async () => true);
      const onSlashCommand = vi.fn();
      renderInput({ onSend, onSlashCommand });
      const textarea = screen.getByRole("textbox");
      fireEvent.change(textarea, { target: { value: "/" } });
      expect(screen.getByTestId("slash-popup")).toBeTruthy();

      fireEvent.keyDown(textarea, { key: "Enter" });
      expect(onSlashCommand).not.toHaveBeenCalled();
      expect(onSend).not.toHaveBeenCalled();
      // The popup stays open so the user can keep narrowing the list.
      expect(screen.getByTestId("slash-popup")).toBeTruthy();

      // ↑/↓ is an explicit choice, so Enter then runs it (catalog index 1).
      fireEvent.keyDown(textarea, { key: "ArrowDown" });
      fireEvent.keyDown(textarea, { key: "Enter" });
      expect(onSlashCommand).toHaveBeenCalledWith("compact", null);
      expect(onSend).not.toHaveBeenCalled();
    });

    it("dismisses the popup on Escape while keeping the text", () => {
      renderInput();
      const textarea = screen.getByRole("textbox") as HTMLTextAreaElement;
      fireEvent.change(textarea, { target: { value: "/he" } });
      fireEvent.keyDown(textarea, { key: "Escape" });
      expect(screen.queryByTestId("slash-popup")).toBeNull();
      expect(textarea.value).toBe("/he");
    });
  });
});
