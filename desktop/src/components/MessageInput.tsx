import { useEffect, useRef, useState, type ReactNode } from "react";
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
  onPickFiles,
  onPickImages,
  onRemoveAttachment,
  threadId = null,
  call,
  disabled = false,
  modelPicker,
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
  /** 用户点了回形针：打开文件选择器是父级的事（Tauri dialog 在 App 里）。 */
  onPickFiles: () => void;
  /** 用户点了「附加图片」：同上，只是白名单、上限与协议形态都不同。 */
  onPickImages: () => void;
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
        className="rounded-lg border border-line-strong bg-surface transition-colors focus-within:border-fg-subtle"
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
                if (e.key === "Enter" && !e.shiftKey) {
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
              if (e.key === "Enter" && !e.shiftKey) {
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
            placeholder="Type a message… (Enter to send, Shift+Enter for newline)"
            // 无边框、透明底的文本区：边框与聚焦指示都归卡片（见上）。卡片内是竖排，
            // 故 textarea 直接占满整行；宽度由 `w-full` 决定，不再需要 `flex-1` / `max-md:*`。
            className="min-w-0 w-full resize-none bg-transparent px-3 pt-2 text-sm text-fg placeholder:text-fg-faint focus:outline-none disabled:opacity-50"
          />
          <div
            data-composer-toolbar
            className="flex items-center justify-between gap-2 px-3 pb-2 max-md:flex-wrap max-md:justify-end"
          >
            <button
              type="button"
              // 纯文本标签（仓库不用 emoji）；`aria-label` 与可见文字一致，是测试锚点。
              aria-label="附加文件"
              title="附加文件"
              className="rounded-md border border-line-strong px-2 py-0.5 text-xs text-fg-muted hover:bg-raised hover:text-fg disabled:opacity-50"
              onClick={onPickFiles}
              disabled={disabled}
            >
              附加文件
            </button>
            <button
              type="button"
              // 图片有自己的入口：白名单（`IMAGE_EXTENSIONS`）与上限（20 MB）都与
              // 文档不同，混在一个选择器里会让用户挑错才被告知。
              aria-label="附加图片"
              title="附加图片"
              className="rounded-md border border-line-strong px-2 py-0.5 text-xs text-fg-muted hover:bg-raised hover:text-fg disabled:opacity-50"
              onClick={onPickImages}
              disabled={disabled}
            >
              附加图片
            </button>
            <div className="flex items-center gap-2">
              <ModeChip mode={mode} onChange={onModeChange} disabled={mode === null} />
              {modelPicker}
              <button
                type="button"
                onClick={turnActive ? onInterrupt : () => void handleSend()}
                disabled={sendDisabled}
                className={
                  turnActive
                    ? "rounded-md bg-red-600 px-4 py-2 text-sm font-medium text-white hover:bg-red-500 disabled:opacity-50"
                    : "rounded-md bg-blue-600 px-4 py-2 text-sm font-medium text-white hover:bg-blue-500 disabled:opacity-50"
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
