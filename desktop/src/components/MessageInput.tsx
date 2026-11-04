import { useEffect, useRef, useState, type ReactNode, type Ref } from "react";
import { AttachmentChips } from "./AttachmentChips";
import { ModeChip } from "./ModeChip";
import { SlashPopup } from "./SlashPopup";
import { filterCommands, parseSlashInput } from "../lib/slash";
import { sideInputPolicy, type SideCommand } from "../lib/sideCommands";
import type { SideSubmitReceipt } from "../lib/sideConversation";
import { isImeEnter, useImeGuard } from "../lib/imeEnter";
import type { ThreadMode } from "../lib/threadPermissionMode";
import type { PendingAttachment } from "../lib/attachmentLimits";
import type { ImageReadCall } from "../lib/useImageData";

/** 输入框所属会话的形状。既有调用点不需要关心它（见 `DEFAULT_CONVERSATION`）。 */
export interface ComposerConversation {
  view: "main" | "side";
  /** 该宿主是否声明了 `side_conversation_v1`。 */
  supported: boolean;
  /** 本设备能否下发 Control 类操作（Observe 不能）。 */
  canControl: boolean;
}

/** 缺省上下文：主会话、宿主支持、可控制。 */
const DEFAULT_CONVERSATION: ComposerConversation = {
  view: "main",
  supported: true,
  canControl: true,
};

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
  conversation = DEFAULT_CONVERSATION,
  onSideCommand = null,
  onSubmitSide = null,
  conversationKey = "",
}: {
  turnActive: boolean;
  onSend: (text: string) => Promise<boolean>;
  onInterrupt: () => void;
  mode: ThreadMode | null;
  onModeChange: (mode: ThreadMode) => void;
  onSlashCommand: (name: string, args: string | null) => boolean | Promise<boolean>;
  /**
   * 文本框内容，由父级按 session 提供（不是本组件的内部状态）。
   *
   * 草稿属于「那个会话」，不属于「这个输入框」：受控后切换 session 时父级直接
   * 换掉 value，上一处的文字留在原 session 的 `draft` 里，不会跟着跑到新会话。
   */
  value: string;
  /** 编辑默认写当前会话；异步清稿显式指定提交时的 threadId。 */
  onDraftChange: (text: string, threadId?: string | null) => void;
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
   * 运行中只显示 Stop。桌面端为 false，运行中也可通过 Enter/Send 提交。
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
  /**
   * 这个输入框属于哪条会话，以及它能不能下发 Control 操作。
   *
   * 缺省是 `main` / 支持 / 可控制 / 无附件：既有调用点逐字节不变。旁支传入
   * `view: "side"`，`supported`/`canControl` 来自该宿主的握手结果。
   *
   * 为什么把 `supported`/`canControl` 放在这里而不是组件去查全局：一个进程
   * 同时连本地与 N 个远端项目，它们的答案各不相同。查「当前连接」的那一份
   * 会把一台只读的远端设备渲染成可写。
   */
  conversation?: ComposerConversation;
  /**
   * 旁支命令的执行入口（`open`/`back`/`close` 与旁支作用域内的状态命令）。
   *
   * 缺省 `null`：没有接旁支的调用点保持原样，旁支命令只会被当成普通文本——
   * 这与改动前的行为一致（`side` 那时不是目录里的命令）。
   */
  onSideCommand?: ((command: SideCommand) => boolean | Promise<boolean>) | null;
  /**
   * 旁支的普通文本提交。返回**三态收据**而不是 boolean。
   *
   * 收据必须留在 controller/store 手里：`accepted` 可以清稿，`rejected` 要把
   * 原文还回去并给原因，`uncertain` 既不能清稿也不能当失败重发（服务端可能已经
   * 受理，重发会让同一个问题被回答两次）。这里压成一个 boolean 就必然会丢掉
   * 其中一种语义——那正是「清掉用户正在写的字」的来源之一。
   *
   * 因此本组件对三种结果都**不**清稿：清稿条件（身份 + 草稿修订全匹配）只有
   * store 知道。
   */
  onSubmitSide?: ((text: string) => Promise<SideSubmitReceipt>) | null;
  /**
   * 当前会话的完整身份（主会话是父身份，旁支是含 generation 的完整序列化）。
   *
   * 只用来隔离本地 latch：一次还在路上的提交，其 `finally` 不能清掉用户切到
   * 另一条会话之后新建的那次提交。
   */
  conversationKey?: string;
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

  // An async receipt belongs to its original thread and draft, not whichever
  // thread happens to be visible when the request resolves.
  const latestDraft = useRef({ threadId, value });
  latestDraft.current = { threadId, value };
  const submitting = useRef(false);
  /**
   * 提交锁归属哪条会话。
   *
   * 一次提交的 `finally` 必须只清掉**它自己那条会话**的锁。否则：用户提交 A，
   * 立刻切到 B 并提交，A 的回包落地时会把 B 的锁也解开——同一条消息就可能在
   * 服务端被受理两次（spec §5.2「双击或重复 Enter 不重复发送」）。
   *
   * 旁支用 store 的收据管清稿，所以这里只负责「别重复发」这一件事。
   */
  const lockOwner = useRef<string | null>(null);
  const holdsLock = () => submitting.current && lockOwner.current === conversationKey;
  const takeLock = (): boolean => {
    if (holdsLock()) return false;
    submitting.current = true;
    lockOwner.current = conversationKey;
    return true;
  };
  const releaseLock = () => {
    if (lockOwner.current !== conversationKey) return;
    submitting.current = false;
    lockOwner.current = null;
  };
  const clearSubmittedDraft = (submittedText: string) => {
    if (latestDraft.current.threadId === threadId && latestDraft.current.value === submittedText) {
      onDraftChange("", threadId);
    }
  };

  const text = value;
  const parsed = parseSlashInput(text);
  /**
   * 这条输入该做什么。**所有入口都问它**：Enter、Send、禁用判定、手机例外。
   *
   * 散落成三处各自判断的话，旁支与手机 Stop-only 会各长出一个例外，而第四个
   * 入口（新加的按钮、补全确认）永远会漏掉其中一个。
   */
  const policy = sideInputPolicy({
    platform: isMobile ? "mobile" : "desktop",
    view: conversation.view,
    running: turnActive,
    attachmentCount: attachments.length,
    text,
    supported: conversation.supported,
    canControl: conversation.canControl,
  });
  /**
   * 有没有一条**可执行**的命令（而不是普通文字）。
   *
   * 手机上运行中只显示 Stop，但旁支命令是显式例外：用户敲了 `/btw ...` 却只
   * 看到 Stop，会以为功能不存在。为真时额外给出一颗「命令」按钮。
   */
  const hasSideCommand =
    policy.action === "executeSideCommand" &&
    (isMobile || conversation.view === "side");
  const isSideView = conversation.view === "side";
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

  /**
   * 旁支里的一次普通提交。
   *
   * 三态收据都不在这里清稿：`accepted` 由 store 按「身份 + 草稿修订」清，
   * `rejected` 把原文留在输入框（用户能直接改），`uncertain` 保持 pending 并
   * 拒绝重复发送。组件层唯一能做的事是「别再发一次」。
   */
  const handleSideSend = async () => {
    if (!text.trim() || disabled || onSubmitSide === null) return;
    if (!takeLock()) return;
    const submitted = text;
    setSending(true);
    try {
      await onSubmitSide(submitted);
    } finally {
      releaseLock();
      setSending(false);
    }
  };

  const handleSend = async () => {
    // 附件本身就是一条消息：只要有文字**或**至少一个附件就可以发；两者都空时
    // 才拦住（空 turn 没有意义）。
    if ((!text.trim() && attachments.length === 0) || disabled) return;
    if (!takeLock()) return;
    setSending(true);
    try {
      const ok = await onSend(text);
      // Only discard the draft once the send was actually accepted; otherwise
      // the user's text would be lost on a rejected turn/start.
      if (ok) clearSubmittedDraft(text);
    } finally {
      releaseLock();
      setSending(false);
    }
  };

  /**
   * Commands and text share a submission lock, including same-tick Enter.
   *
   * 守卫不再在这里判「手机运行中」：那条规则属于策略（见 `policy`），在这里
   * 再判一次就会与策略分叉——旁支命令恰恰是手机运行中的合法例外。
   */
  const runSlash = async (name: string, args: string | null) => {
    if (disabled) return;
    if (!takeLock()) return;
    setSending(true);
    try {
      if (await onSlashCommand(name, args)) {
        clearSubmittedDraft(text);
        setPopupOpen(false);
      }
    } finally {
      releaseLock();
      setSending(false);
    }
  };

  /** 执行旁支命令。**所有**里层命令都交给 `onSideCommand`，绝不溜回父 slash。 */
  const runSideCommand = async (command: SideCommand) => {
    if (disabled) return;
    if (command.kind === "back") {
      // 返回是纯本地动作：不占提交锁、不看 `sending`、不等任何在途请求。
      // 把它排到一次提交后面就等于「网络慢的时候回不去」（spec §4.2）。
      //
      // 受理后仍要清掉**这次提交的命令文本**（与 `runSlash` 同一条规则）：留着
      // `/back` 会让用户下次进来看到输入框里躺着一句命令。清稿条件仍是「正文
      // 与提交时完全一致」，所以不会动他没提交过的编辑。
      if (await onSideCommand?.(command)) clearSubmittedDraft(text);
      return;
    }
    if (!takeLock()) return;
    setSending(true);
    try {
      // `command` 分支（help/config/usage）也必须走 `onSideCommand`：它读的是
      // 旁支固定的脱敏配置与旁支自己的用量。转给 `onSlashCommand` 会把父的
      // 模型和父的费用显示成旁支的（spec §4.2）。
      const accepted = await (onSideCommand?.(command) ?? false);
      if (accepted) {
        clearSubmittedDraft(text);
        setPopupOpen(false);
      }
    } finally {
      releaseLock();
      setSending(false);
    }
  };

  /**
   * 策略拦截时的一句话。不清稿、不发请求。
   *
   * 必须**可见**：用户敲了 `/clear` 却什么都没发生时，唯一能解释原因的地方
   * 就是这里。语义上不是错误，所以用中性文字而不是红色横幅。
   */
  const blockedReason = policy.action === "blocked" ? (policy.reason ?? null) : null;

  /**
   * Enter, Send 与手机上的命令按钮**共用**这一条分派。
   *
   * 顺序是：先问策略，再决定是命令、旁支还是普通消息。命令永远不能因为用户
   * 点了按钮而不是按回车就漏给模型——那是「点了 /clear 结果模型收到一段
   * 『/clear』」的由来。
   */
  const handleSendClick = () => {
    if (policy.action === "stop" || policy.action === "blocked") return;
    if (policy.action === "executeSideCommand" && policy.command) {
      void runSideCommand(policy.command);
      return;
    }
    if (parsed.kind === "command") {
      // Desktop completion applies equally to Enter and Send. Mobile retains
      // its existing exact-command semantics because it has no Enter-to-send.
      if (!isMobile && showPopup && options.length > 0) {
        const picked = options[Math.min(selected, options.length - 1)];
        void runSlash(picked.name, null);
      } else {
        void runSlash(parsed.name, parsed.args);
      }
      return;
    }
    if (text.trim() === "/") {
      if (!isMobile && showPopup && selected > 0 && options.length > 0) {
        void runSlash(options[Math.min(selected, options.length - 1)].name, null);
      }
      return;
    }
    if (isSideView && onSubmitSide !== null) {
      // 旁支正文走收据接口：清稿由 store 按身份与草稿修订裁决，不经父 onSend
      // 那条「true 就清稿」的分支（那会清掉 await 期间的新编辑）。
      void handleSideSend();
      return;
    }
    void handleSend();
  };

  // 「没什么可发」时按钮就该是灰的，而不是点了没反应：文字与附件都是空时才算空；
  // 附件已在路上时按钮要亮着，否则「只发文件」这条路径在 UI 上根本走不通。
  //
  // 旁支里「运行中」时 Send 换成本会话的 Stop（见下方渲染）；策略判 `stop`
  // 时只保留 Stop，普通文字不得插话。
  //
  // 旁支里不用 `sending` 禁用输入区：一次提交还在路上时用户仍要能编辑、能打
  // `/back`、能点 Stop。「别重复发」由 `takeLock` 按会话身份保证，而不是把整个
  // 编辑器冻住（spec 全局约束「操作锁不禁用整个编辑器」）。
  const sendDisabled =
    disabled ||
    (sending && !isSideView) ||
    (!text.trim() && attachments.length === 0) ||
    policy.action === "stop" ||
    policy.action === "blocked";
  // Stop 只受「这个输入区是否可用」约束：`sending` 表示**这次发送还没回包**，
  // 与「服务端这一轮是否在跑」无关——turn/started 可能早已到达、bash 可能已经在
  // 输出，而 Promise 仍挂着。把它算进来会让 Stop 在这段时间里点不动（真机上常见
  // 的「点了没反应」）。同理内容为空也不该灰：打断与有没有内容无关。
  const stopDisabled = disabled;

  return (
    <div className="relative border-t border-line bg-panel p-3">
      {showPopup && <SlashPopup commands={options} selected={selected} />}
      {blockedReason !== null && (
        // 明确拒绝必须可见：命令没执行、稿也没清，用户唯一能知道原因的途径
        // 就是这一行（spec §5.3「错误、未确定提交、关闭冲突和权限不足属于操作
        // 反馈，在现有消息/错误位置按需显示，无事件时不占位」）。
        <p role="status" className="mb-2 px-1 text-xs text-amber-600">
          {blockedReason}
        </p>
      )}
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
              // Desktop commands remain available mid-turn; mobile keeps its
              // Stop-only running state.
              setPopupOpen((!turnActive || !isMobile) && /^\/[^\s/]*$/.test(next.trim()));
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
                  handleSendClick();
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
                handleSendClick();
              }
            }}
            // Active turns stay editable. Only an in-flight submission or the
            // parent availability guard disables the textarea.
            // 不用 `sending` 冻结文本区：一次提交还在路上时用户必须仍能编辑、
            // 打 `/back`、点 Stop。重复发送由 `takeLock` 按会话身份拦住。
            disabled={disabled}
            rows={3}
            placeholder={
              isSideView
                ? isMobile
                  ? "旁问 · 输入问题（Enter 换行，点发送）· /back 返回"
                  : "旁问 · 输入问题（Enter 发送，Shift+Enter 换行）· /back 返回"
                : isMobile
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
              {/*
               * **一个主动作**。手机上尤其重要：软键盘没有 Enter-to-send，用户
               * 唯一的出口就是这颗按钮，所以它必须根据输入自己变成「执行命令 /
               * Stop / Send」，而不是并排三个按钮让用户猜该点哪个。
               *
               * 顺序就是策略的顺序：
               * 1. 有一条**可执行**命令 → 执行它（桌面与手机都一样；这条是手机
               *    Stop-only 的精确例外，`/btw ...`、`/back` 都走这里）。
               * 2. 该轮在跑 → Stop（旁支里是旁支自己的 Stop）。
               * 3. 否则 → Send。
               */}
              {/*
               * Stop 与 Send 是**两颗独立**的按钮（桌面如此，也是既有行为）：
               * 运行中仍然可以投递插话，同时 Stop 必须随手可点。
               *
               * 手机只留**一个主动作**：软键盘没有 Enter-to-send，用户唯一的出口
               * 就是这颗按钮，所以它按输入自己变成「执行命令 / Stop / Send」。
               * 也不能靠前缀（`/b`）绕过 Stop-only——只有**精确可执行**的命令
               * 才切走 Stop（`policy.action === "executeSideCommand"`）。
               *
               * 任何情况下都不再有第三颗「命令」按钮：命令与发送共用同一条分派
               * 与同一个主动作（spec §4.2）。
               */}
              {turnActive && (
                <button
                  type="button"
                  onClick={onInterrupt}
                  disabled={stopDisabled}
                  className="rounded-md bg-red-600 px-4 py-2 text-sm font-medium text-white hover:bg-red-500 disabled:opacity-50 max-md:ml-auto"
                >
                  Stop
                </button>
              )}
              {/*
               * 旁支运行中：只有它自己的 Stop。文字仍可编辑，但「可编辑」不等于
               * 「可发送」——旁支不插话、不排队（spec §4.2）。这里不能因为桌面
               * 端主会话可以插话，就把 Send 也留给旁支。
               */}
              {/* 手机：命令与 Stop 共用同一颗主动作（不并排第三颗按钮）。 */}
              {isSideView && turnActive ? null : isMobile && hasSideCommand ? (
                <button
                  type="button"
                  onClick={handleSendClick}
                  disabled={disabled}
                  className="rounded-md bg-blue-600 px-4 py-2 text-sm font-medium text-white hover:bg-blue-500 disabled:opacity-50 max-md:ml-auto"
                >
                  执行命令
                </button>
              ) : (
                (!isMobile || !turnActive) && (
                  <button
                    type="button"
                    onClick={handleSendClick}
                    disabled={sendDisabled}
                    className="rounded-md bg-blue-600 px-4 py-2 text-sm font-medium text-white hover:bg-blue-500 disabled:opacity-50 max-md:ml-auto"
                  >
                    Send
                  </button>
                )
              )}
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}
