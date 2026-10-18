import { useEffect, useRef, useState, type ReactNode, type Ref } from "react";
import { AttachmentChips } from "./AttachmentChips";
import { ModeChip } from "./ModeChip";
import { SlashPopup } from "./SlashPopup";
import { filterCommands, parseSlashInput } from "../lib/slash";
import { isImeEnter, useImeGuard } from "../lib/imeEnter";
import type { ThreadMode } from "../lib/threadPermissionMode";
import type { PendingAttachment } from "../lib/attachmentLimits";
import type { ImageReadCall } from "../lib/useImageData";

export function MessageInput({
  turnActive,
  onSend,
  onInterrupt,
  mode,
  onModeChange,
  onSlashCommand,
  value,
  onDraftChange,
  attachments,
  onPickAttachments,
  isMobile = false,
  onRemoveAttachment,
  threadId = null,
  call,
  disabled = false,
  modelPicker,
  cardRef,
  dropActive = false,
  caretSeek = null,
  onCaretChange,
}: {
  turnActive: boolean;
  onSend: (text: string) => Promise<boolean>;
  onInterrupt: () => void;
  mode: ThreadMode | null;
  onModeChange: (mode: ThreadMode) => void;
  onSlashCommand: (name: string, args: string | null) => void;
  /**
   * 文本框内容，由父级按 session 提供（不是本组件的内部状态）。
   *
   * 草稿属于「那个会话」，不属于「这个输入框」：受控后切换 session 时父级直接
   * 换掉 value，上一处的文字留在原 session 的 `draft` 里，不会跟着跑到新会话。
   */
  value: string;
  /** 用户每次改动文本框；父级把它写进当前 session 的 `draft`。 */
  onDraftChange: (text: string) => void;
  /**
   * 当前会话待发送的附件，父级按 session 保管（与 `value` 同理：附件属于会话）。
   *
   * 移除与「发送成功后清空」都由父级做：前者是父级的状态，后者是父级才知道的
   * 知识（`onSend` 返回 true 才代表服务端收下了这一轮）。
   */
  attachments: PendingAttachment[];
  /**
   * 用户点了回形针：打开文件选择器是父级的事（Tauri dialog 在 App 里）。
   *
   * 文档与图片共用一个入口：一次选择里可以混选，分类由父级按扩展名判定（见
   * `App` 的 `pickAttachments`）。两个白名单互斥，故路径的扩展名足以分流。
   */
  onPickAttachments: () => void;
  /**
   * 手机端（配对 iPhone / iOS webview）：软键盘没有 Shift 键，回车恒为
   * `shiftKey === false`。此时把回车让给「换行」，发送与命令执行收敛到 Send 按钮；
   * 桌面端为 `false`，行为与文案逐字节不变。
   */
  isMobile?: boolean;
  /** 用户移除了某个 chip；参数是附件的本地路径。 */
  onRemoveAttachment: (path: string) => void;
  /**
   * 当前会话 id 与图片读取接缝，只为待发图片 chip 上的缩略图存在；原样透传给
   * `AttachmentChips`。`call` **必填**（此前可选并带 `NO_READ` 兜底，会让漏注入
   * 时静默降级成失败 chip）且**必须稳定引用**（`App` 的 `imageCall`），否则每张
   * 缩略图都会随每次渲染重新分片拉取。
   */
  threadId?: string | null;
  call: ImageReadCall;
  /**
   * 没有当前 session 时置灰整个输入区（App 不会无会话渲染它，这一层是护栏）：
   * 否则会留下一个能敲字、却因无处存放草稿而静默丢字的文本框。
   */
  disabled?: boolean;
  /**
   * 追加在工具栏里的控件（与 ModeChip / 发送按钮同一区域，居发送按钮左侧）。当前由
   * App 传入会话级模型下拉；`MessageInput` 不关心它是什么，只负责给它一个位置。
   */
  modelPicker?: ReactNode;
  /**
   * 卡片元素本身的 ref：上层（`App` 的拖放落点判定）需要卡片的矩形来算"拖到这里
   * 才算数"。DOM 归属仍在本组件，故由本组件挂到卡片上，而不是让上层去查 DOM。
   */
  cardRef?: Ref<HTMLDivElement>;
  /**
   * 拖拽悬停在卡片上：整卡加一圈高亮，让"这里可以放"一眼可见。
   *
   * 用 `ring`（box-shadow）而非换 `border-*` 类：基础类里的 `border-line-strong`
   * 是本卡边框的唯一来源（既有测试逐字节断言它），再叠一条 `border-*` 谁生效取决于
   * 生成样式顺序，不可靠。
   */
  dropActive?: boolean;
  /**
   * 请求把光标放到 `pos`。带 `seq` 是为了让**连续两次落在同一位置**也能各生效一次
   * ——否则两次拖入的参数对象相等，effect 依赖判不出变化。
   */
  caretSeek?: { seq: number; pos: number } | null;
  /**
   * 用户当前的光标位置，报给上层。上层记着它，拖入时才知道该往哪里插。
   */
  onCaretChange?: (caret: number) => void;
}) {
  const [sending, setSending] = useState(false);
  // Latch: whether the popup should currently be offered. Escape clears it to
  // dismiss; typing re-arms it from the text's own shape (see onChange).
  const [popupOpen, setPopupOpen] = useState(false);
  const [selected, setSelected] = useState(0);
  const inputRef = useRef<HTMLTextAreaElement>(null);
  // Enter that confirms an IME candidate must not send the message; see
  // `isImeCompositionKey` for why keyCode 229 is the load-bearing check here.
  const ime = useImeGuard();

  const text = value;
  const parsed = parseSlashInput(text);
  // Command names contain no spaces (the catalog has no argument-bearing
  // names), so a space means the caret left the name for the argument list and
  // the popup closes. A bare `/` opens the popup too, even though
  // `parseSlashInput("/")` is `{ kind: "none" }` — a lone slash names no
  // command but is still the moment the user asked for the menu.
  const typingName = /^\/[^\s/]*$/.test(text.trim());
  const showPopup = popupOpen && typingName;
  const options = showPopup ? filterCommands(parsed.kind === "command" ? parsed.name : "") : [];

  useEffect(() => {
    setSelected(0);
  }, [text]);

  // 上层插入路径后请求把光标放回插入点：受控 textarea 的文本由父级换掉，光标
  // 会跳到末尾，不这样摆一次，用户就得手动点回来。
  useEffect(() => {
    if (caretSeek === null) return;
    const el = inputRef.current;
    if (!el) return;
    el.focus();
    el.setSelectionRange(caretSeek.pos, caretSeek.pos);
  }, [caretSeek]);

  const handleSend = async () => {
    // 附件本身就是一条消息：只要有文字**或**至少一个附件就可以发；两者都空时
    // 才拦住（空 turn 没有意义）。
    if ((!text.trim() && attachments.length === 0) || sending) return;
    setSending(true);
    try {
      const ok = await onSend(text);
      // Only discard the draft once the send was actually accepted; otherwise
      // the user's text would be lost on a rejected turn/start.
      if (ok) onDraftChange("");
    } finally {
      setSending(false);
    }
  };

  /** Run the slash command the input currently names (or report it unknown). */
  const runSlash = (name: string, args: string | null) => {
    onSlashCommand(name, args);
    onDraftChange("");
    setPopupOpen(false);
  };

  /**
   * Send 按钮的点击处理。
   *
   * 桌面端就是 `handleSend`。手机端回车已让给换行，命令（`/cost` 等）的入口必须补
   * 到 Send 上：草稿解析为命令就先执行它。裸 `/`（`parsed.kind === "none"` 但弹窗
   * 开着）按桌面端回车的既有语义处理为 inert——弹窗默认高亮是破坏性的 `/clear`，
   * 未经用户选择不得执行。
   */
  const handleSendClick = () => {
    if (isMobile && parsed.kind === "command") {
      runSlash(parsed.name, parsed.args);
      return;
    }
    if (isMobile && showPopup && options.length > 0) return;
    void handleSend();
  };

  // 「没什么可发」时按钮就该是灰的，而不是点了没反应：文字与附件都是空时才算空；
  // 附件已在路上时按钮要亮着，否则「只发文件」这条路径在 UI 上根本走不通。
  // Stop 态不用额外分支：那时 `turnActive` 为真，本式已退化成 `disabled || sending`
  // ——打断的意义与有没有内容无关。
  const sendDisabled =
    disabled || sending || (!turnActive && !text.trim() && attachments.length === 0);

  return (
    <div className="relative border-t border-line bg-panel p-3">
      {showPopup && <SlashPopup commands={options} selected={selected} />}
      {/*
       * 输入卡片：整卡承载边框、圆角与底色，聚焦（textarea 或卡内任意控件）时整卡高亮。
       * 边框从 textarea 搬到卡片上——这样"输入的地方"是一个整体，而不是一块输入框加
       * 一列散在框外的控件。
       */}
      <div
        ref={cardRef}
        data-drop-active={dropActive ? "" : undefined}
        className={
          "rounded-lg border border-line-strong bg-surface transition-colors focus-within:border-fg-subtle" +
          (dropActive ? " ring-2 ring-blue-500" : "")
        }
        data-testid="composer-card"
      >
        <AttachmentChips
          attachments={attachments}
          onRemove={onRemoveAttachment}
          threadId={threadId}
          call={call}
          // 输入框上方的 chip 恒为待发：路径是 OS dialog 的绝对路径，`image/read`
          // 读不回来是预期内的，缩略图走中性占位（见 `AttachmentThumb` 的限制说明）。
          pending
        />
        {/*
         * 卡片内**恒为竖排**：textarea 在上，工具栏是它下面满宽的一行。
         * 不能用 `flex items-end gap-2`（旧布局的两列并排）——那样工具栏只占自身内容宽，
         * `justify-between` 便无空间可分配，「附加文件」会被挤到右端（实测：并排时距卡
         * 左缘 924px，竖排时 13px）。jsdom 断的是类名而非几何，故这条靠结构契约守住。
         */}
        <div className="flex flex-col">
          <textarea
            ref={inputRef}
            value={text}
            onChange={(e) => {
              const next = e.target.value;
              onDraftChange(next);
              // Re-arm the popup whenever the text still looks like a command name.
              setPopupOpen(/^\/[^\s/]*$/.test(next.trim()));
            }}
            onCompositionStart={ime.onCompositionStart}
            onCompositionEnd={ime.onCompositionEnd}
            onBlur={ime.resetComposition}
            onSelect={(e) => onCaretChange?.(e.currentTarget.selectionStart)}
            onClick={(e) => onCaretChange?.(e.currentTarget.selectionStart)}
            onKeyUp={(e) => onCaretChange?.(e.currentTarget.selectionStart)}
            onKeyDown={(e) => {
              if (isImeEnter(e, ime.composing.current)) return;
              if (showPopup && options.length > 0) {
                if (e.key === "ArrowDown") {
                  e.preventDefault();
                  setSelected((i) => (i + 1) % options.length);
                  return;
                }
                if (e.key === "ArrowUp") {
                  e.preventDefault();
                  setSelected((i) => (i - 1 + options.length) % options.length);
                  return;
                }
                if (e.key === "Tab") {
                  e.preventDefault();
                  const picked = options[Math.min(selected, options.length - 1)];
                  onDraftChange(`/${picked.name} `);
                  setPopupOpen(false);
                  inputRef.current?.focus();
                  return;
                }
                if (e.key === "Escape") {
                  e.preventDefault();
                  setPopupOpen(false);
                  return;
                }
                if (e.key === "Enter" && !e.shiftKey && !isMobile) {
                  e.preventDefault();
                  // A bare "/" names no command (`parseSlashInput("/")` is
                  // `{ kind: "none" }`), yet the popup is offered for it and its
                  // DEFAULT highlight is index 0 — the destructive `/clear`. A stray
                  // Enter on that untouched default would erase the transcript, so it
                  // is inert until the user either types a name character (kind turns
                  // "command", as for "/cos") or moves the highlight with ↑/↓ (an
                  // explicit choice, which the line below honours). Tab still
                  // completes.
                  if (parsed.kind !== "command" && selected === 0) return;
                  // A space closes the popup (name-mode only), so no arguments can
                  // be pending here: accepting the highlighted row is exactly what
                  // "complete and run" means (`/cos` -> `/cost`). Fully typed
                  // commands — arguments and unknowns included — reach the
                  // no-popup branch below with `parsed` intact.
                  const picked = options[Math.min(selected, options.length - 1)];
                  runSlash(picked.name, null);
                  return;
                }
                return; // 弹窗开启时吞掉其余按键,不作文本处理
              }
              if (e.key === "Escape" && popupOpen) {
                e.preventDefault();
                setPopupOpen(false);
                return;
              }
              if (e.key === "Enter" && !e.shiftKey && !isMobile) {
                // Confirming a candidate with Enter is the IME's key, not the user's:
                // let the composition land in the box and wait for the next Enter.
                e.preventDefault();
                if (parsed.kind === "command") {
                  // This branch is reached when the popup matched nothing: unknown
                  // commands and matched commands alike belong to the command
                  // layer, never to the agent (`/nope` reports, it does not send).
                  runSlash(parsed.name, parsed.args);
                  return;
                }
                if (parsed.kind === "path") {
                  // Two-slash first token is a path (TUI parity) — falls through.
                } else if (turnActive) {
                  onInterrupt();
                  return;
                }
                void handleSend();
              }
            }}
            disabled={disabled || sending || turnActive}
            rows={3}
            placeholder={
              isMobile
                ? "Type a message… (Enter for newline, tap Send to send)"
                : "Type a message… (Enter to send, Shift+Enter for newline)"
            }
            // 无边框、透明底的文本区：边框与聚焦指示都归卡片（见上）。卡片内是竖排，
            // 故 textarea 直接占满整行；宽度由 `w-full` 决定，不再需要 `flex-1` / `max-md:*`。
            className="min-w-0 w-full resize-none bg-transparent px-3 pt-2 text-sm text-fg placeholder:text-fg-faint focus:outline-none disabled:opacity-50"
          />
          {/*
           * 手机端工具栏：**两行**——第一行「附加文件」，第二行 Mode / 模型 / Send，
           * 其中 Send 贴右缘（`max-md:ml-auto`，见下）。桌面端不匹配任何 `max-md:`，
           * 仍是「附加文件靠左、mode 组靠右」的一行，逐字节不变。
           *
           * 为什么不能给整组 `max-md:justify-end`：这三颗控件都有「不可压缩」的内容
           * ——模型名可长到 `max-w-[16rem]`（256px）、Send 是 `px-4` 的实心按钮——而
           * ModeChip 恰好是这一行**最左**的元素。`justify-end` 会让溢出的空白从右侧
           * 「吸」走，把最左那颗（ModeChip）连同它 `left-0` 的下拉一起**推出屏幕左
           * 边界**，用户根本点不到模式；`start` 对齐则让溢出只落在右侧。所以右对齐落在
           * 单颗 Send 上（`ml-auto`），而不是整组。承载 ModeChip 的那一组同时开
           * `max-md:flex-wrap` + `max-md:min-w-0` + `max-md:basis-full`（见下）：整组
           * 独占第二行，其最小内容宽不再撑破屏幕，模型名也被单独收紧到 8rem
           * （`ModelPicker` 的 `max-md:max-w-[8rem]`），使这三件在窄屏同一行放得下。
           */}
          <div
            data-composer-toolbar
            className="flex items-center justify-between gap-2 px-3 pb-2 max-md:flex-wrap max-md:justify-start"
          >
            <button
              type="button"
              // 纯文本标签（仓库不用 emoji）；`aria-label` 与可见文字一致，是测试锚点。
              //
              // 文档与图片共用这一个入口：一次选择里可以混选，父级按扩展名分流（两个
              // 白名单互斥）。合并的是「入口」，不是「预检」——各自的上限与协议形态
              // 在 App 与发送端照旧。
              aria-label="附加文件"
              title="附加文件"
              className="rounded-md border border-line-strong px-2 py-0.5 text-xs text-fg-muted hover:bg-raised hover:text-fg disabled:opacity-50"
              onClick={onPickAttachments}
              disabled={disabled}
            >
              附加文件
            </button>
            <div className="flex items-center gap-2 max-md:flex-wrap max-md:min-w-0 max-md:basis-full">
              <ModeChip mode={mode} onChange={onModeChange} disabled={mode === null} />
              {modelPicker}
              <button
                type="button"
                onClick={turnActive ? onInterrupt : handleSendClick}
                disabled={sendDisabled}
                className={
                  turnActive
                    ? "rounded-md bg-red-600 px-4 py-2 text-sm font-medium text-white hover:bg-red-500 disabled:opacity-50 max-md:ml-auto"
                    : "rounded-md bg-blue-600 px-4 py-2 text-sm font-medium text-white hover:bg-blue-500 disabled:opacity-50 max-md:ml-auto"
                }
              >
                {turnActive ? "Stop" : "Send"}
              </button>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}
