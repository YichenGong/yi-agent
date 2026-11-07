/**
 * 底部常驻消息区：受控 textarea + 发送/停止，以及原位停止确认。
 *
 * 这里是**受控**组件：草稿正文、三种 pending、确认令牌、提示与错误全部来自
 * `TraceActionSnapshot`，自己只保留「这一栏谁在按 Enter」这类瞬时 UI 状态。
 * 之所以不给它内部草稿：一次发送的成功/失败必须只清**提交时的那个版本**，
 * 判断依据（revision）只有拥有者手里有；放两份就会在切任务、迟到回包时打架。
 *
 * 四件容易做错的事，逐条对应下面的实现：
 *
 * 1. **输入法**。Enter 属于候选词确认而不是发送；WKWebView 在确认候选词时
 *    会把 composition 先拆掉再送 keydown，那一次只能靠 `keyCode === 229` 认出来，
 *    所以 compositionend 之后仍要继续拦。`onBlur` 必须复位守卫——失焦可能不派
 *    `compositionend`，锁住的守卫会永久吞掉之后所有 Enter。
 * 2. **在途仍可编辑**。`messagePending` 禁的是「再发一次」，不是让人打不了下一段。
 * 3. **同步重复守卫**。React 的 pending 属性是异步到达的，双击/连按 Enter 之间
 *    还有一帧空隙，所以另用一个 ref 做同步闸门。
 * 4. **确认区替代动作行，但不卸载 textarea**。焦点与草稿都属于输入区。
 */
import { useEffect, useLayoutEffect, useRef } from "react";

import { isImeEnter, useImeGuard } from "../lib/imeEnter";
import type { TraceActionSnapshot } from "../lib/subagentTraceActions";

export interface TraceComposerProps {
  value: TraceActionSnapshot;
  /** The row's current execution state is terminal: no stop control any more. */
  finished: boolean;
  onDraftChange(text: string): void;
  onSend(): Promise<void>;
  onPreview(): Promise<void>;
  onConfirm(): Promise<void>;
  onDismiss(): void;
  /** Where focus goes when nothing in this bar survives (the task heading). */
  onFallbackFocus(): void;
}

/** 两行起步，最多四行；再长就在控件内滚动（这不算第二个正文滚动区）。 */
const MIN_ROWS = 2;
const MAX_ROWS = 4;

const NOTHING_TO_CANCEL = "当前没有可取消的任务";
const EXPIRED = "确认已过期，请重新预览";

function taskCount(ids: readonly string[]): number {
  return new Set(ids).size;
}

export function TraceComposer({
  value,
  finished,
  onDraftChange,
  onSend,
  onPreview,
  onConfirm,
  onDismiss,
  onFallbackFocus,
}: TraceComposerProps) {
  const ime = useImeGuard();
  /**
   * 同步闸门：从按下 Enter 到 React 把 `messagePending` 送回来之间还有一帧，
   * 连按两下就会在这个缝里发出两条。ref 在同一 tick 内就拦住第二次。
   */
  const sendingRef = useRef(false);
  const previewingRef = useRef(false);
  const textareaRef = useRef<HTMLTextAreaElement | null>(null);
  const stopRef = useRef<HTMLButtonElement | null>(null);
  /**
   * 确认区消失时焦点该去哪。
   *
   * 只在「消失前焦点确实在确认区里」时才还给停止按钮：否则成功/终态通知会
   * 把正在打字的用户从 textarea 里拽出来。
   */
  const focusInConfirmRef = useRef(false);

  const confirmation = value.confirmation;
  const expired = confirmation !== null && confirmation.deadline <= Date.now();
  const scope = confirmation === null ? 0 : taskCount(confirmation.taskIds);
  const blank = value.draft.trim().length === 0;

  useEffect(() => {
    if (!value.messagePending) sendingRef.current = false;
  }, [value.messagePending]);

  useEffect(() => {
    if (!value.previewPending) previewingRef.current = false;
  }, [value.previewPending]);

  const confirmRef = useRef<HTMLDivElement | null>(null);
  const dismissRef = useRef<HTMLButtonElement | null>(null);
  const rootRef = useRef<HTMLDivElement | null>(null);
  /**
   * 焦点上一次是否落在这一栏里。
   *
   * 不能等确认区出现后再读 `document.activeElement`：那时「停止任务」已经被替换
   * 卸掉，浏览器把焦点退回 body，读到的永远是「不在」。所以在隆起的 focus/blur
   * 里记，卸载不派发 blur 也不会把标记清掉。
   */
  const rootHadFocusRef = useRef(false);
  /** 上一次提交时确认区是否开着，用来认出「刚刚被替换掉」这一帧。 */
  const confirmWasOpenRef = useRef(false);

  /** 确认区开着时，把「焦点是否在里面」记下来，供关闭时决定去处。 */
  const noteFocusInsideConfirm = () => {
    const active = document.activeElement;
    const region = confirmRef.current;
    focusInConfirmRef.current = Boolean(region && active && region.contains(active));
  };

  /**
   * 焦点离开确认区时要复位。
   *
   * 只在 `onFocus` 里置位会留下陈旧标记：程序把焦点交给「放弃」之后再点回
   * textarea，标记仍是 true，确认区一消失就把人从输入框拽到停止按钮上。
   */
  const dropFocusInConfirm = (event: React.FocusEvent<HTMLDivElement>) => {
    const next = event.relatedTarget as Node | null;
    focusInConfirmRef.current = next !== null && confirmRef.current?.contains(next) === true;
  };

  useLayoutEffect(() => {
    const open = confirmation !== null;
    const wasOpen = confirmWasOpenRef.current;
    confirmWasOpenRef.current = open;
    if (open && !wasOpen) {
      // 键盘用户先落到「放弃」：默认焦点停在危险动作上时，一次 Enter 就停掉了任务。
      // 只有当焦点**本来就在这一栏**（用户点了「停止任务」）时才移交；从别处
      // 抢焦点会把正在打字的用户拽走。
      if (rootHadFocusRef.current) {
        rootHadFocusRef.current = false;
        dismissRef.current?.focus({ preventScroll: true });
      }
      return;
    }
    if (!wasOpen || open) return;
    // 确认区刚被替换掉。只有焦点原本确实在里面时才移动焦点，否则会从
    // 正在打字的 textarea（或别的输入框）里把人拽走。
    const had = focusInConfirmRef.current;
    focusInConfirmRef.current = false;
    if (!had) return;
    // 终态会让停止按钮一起消失，此时没有可回的地方，交给调用方（标题）。
    if (finished || !stopRef.current) {
      onFallbackFocus();
      return;
    }
    stopRef.current.focus({ preventScroll: true });
  }, [confirmation, finished, onFallbackFocus]);

  const send = () => {
    if (blank) return;
    if (sendingRef.current || value.messagePending) return;
    sendingRef.current = true;
    // 错误由拥有者记录并展示；这里的 rejection 只是「这条路走完了」。
    void onSend().catch(() => {});
  };

  const preview = () => {
    if (previewingRef.current || value.previewPending || value.confirmPending) return;
    previewingRef.current = true;
    void onPreview().catch(() => {});
  };

  return (
    <div
      ref={rootRef}
      onFocus={() => {
        rootHadFocusRef.current = true;
      }}
      onBlur={(event) => {
        const next = event.relatedTarget as Node | null;
        rootHadFocusRef.current = next !== null && rootRef.current?.contains(next) === true;
      }}
      className="shrink-0 border-t border-line px-3 py-2"
    >
      {value.error !== null ? (
        <p role="alert" className="mb-1 text-xs text-danger">
          {value.error}
        </p>
      ) : null}
      {/* 只有一个 polite 状态区：多条提示挤在一起会互相打断屏幕阅读器。 */}
      <p role="status" aria-live="polite" className="mb-1 min-h-4 text-xs text-fg-subtle">
        {value.notice ?? ""}
      </p>

      <div className="flex items-end gap-2">
        <label className="sr-only" htmlFor="subagent-message">
          向这个 agent 发送消息
        </label>
        <textarea
          id="subagent-message"
          ref={textareaRef}
          aria-label="向这个 agent 发送消息"
          rows={MIN_ROWS}
          value={value.draft}
          onChange={(event) => onDraftChange(event.target.value)}
          onCompositionStart={ime.onCompositionStart}
          onCompositionEnd={ime.onCompositionEnd}
          onBlur={ime.resetComposition}
          onKeyDown={(event) => {
            if (event.key !== "Enter") return;
            if (event.shiftKey) return;
            if (isImeEnter(event, ime.composing.current)) return;
            event.preventDefault();
            send();
          }}
          className="min-h-0 min-w-0 flex-1 resize-none rounded border border-line bg-panel px-2 py-1 text-sm text-fg"
          style={{ maxHeight: `${MAX_ROWS * 1.5 + 0.5}rem` }}
        />

        {confirmation === null ? (
          <div className="flex shrink-0 flex-col items-end gap-1">
            <button
              type="button"
              disabled={blank || value.messagePending}
              onClick={send}
              className="rounded border border-line px-2 py-1 text-xs text-fg-muted hover:text-fg disabled:opacity-50"
            >
              发送
            </button>
            {!finished ? (
              <button
                type="button"
                ref={stopRef}
                disabled={value.previewPending || value.confirmPending}
                onClick={preview}
                className="text-xs text-danger hover:opacity-80 disabled:opacity-50"
              >
                停止任务
              </button>
            ) : null}
          </div>
        ) : (
          /* 确认区取代动作行，但 textarea 保留：草稿与焦点都属于输入区。 */
          <div
            ref={confirmRef}
            onFocus={noteFocusInsideConfirm}
            onBlur={dropFocusInConfirm}
            className="flex max-w-[60%] shrink-0 flex-col items-end gap-1 rounded border border-line bg-panel px-2 py-1.5"
          >
            <span className="text-xs text-fg">
              {scope === 0
                ? NOTHING_TO_CANCEL
                : scope === 1
                  ? "停止该任务？"
                  : `将停止 ${scope} 个任务`}
            </span>
            {expired ? <span className="text-xs text-warning">{EXPIRED}</span> : null}
            <div className="flex items-center gap-2">
              <button
                type="button"
                disabled={scope === 0 || expired || value.confirmPending}
                onClick={() => {
                  noteFocusInsideConfirm();
                  void onConfirm().catch(() => {});
                }}
                className="text-xs text-danger hover:opacity-80 disabled:opacity-50"
              >
                确认停止
              </button>
              <button
                type="button"
                ref={dismissRef}
                disabled={value.confirmPending}
                onClick={() => {
                  noteFocusInsideConfirm();
                  onDismiss();
                }}
                className="text-xs text-fg-muted hover:text-fg disabled:opacity-50"
              >
                放弃
              </button>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
