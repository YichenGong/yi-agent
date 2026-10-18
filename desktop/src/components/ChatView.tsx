import { memo, useEffect, useState } from "react";
import type { Item, RetryCause } from "../lib/protocol";
import { fileNameOf } from "../lib/attachmentLimits";
import type { NoticeItem, ReasoningItem } from "../lib/session";
import { useImageData, type ImageReadCall } from "../lib/useImageData";
import { useStickToBottom } from "../lib/useStickToBottom";
import { useThrottledText } from "../lib/useThrottledText";
import { AgentMessage } from "./MarkdownText";
import { ConfigCard } from "./ConfigCard";
import { ToolCallCard } from "./ToolCallCard";

/**
 * 一张图片引用（path → 分片读取 → 对象 URL）。`refs` 由服务端下发、长度固定，
 * 依赖下标是安全的。
 */
function BubbleImage({
  path,
  threadId,
  call,
}: {
  path: string;
  threadId: string | null;
  call: ImageReadCall;
}) {
  const { url, error } = useImageData(path, { threadId, call });
  if (error) {
    return (
      <div className="rounded border border-red-400/40 bg-red-500/10 px-2 py-1 text-xs text-red-200">
        图片读取失败：{error}
      </div>
    );
  }
  if (!url) return null;
  return (
    <img
      src={url}
      alt="attached"
      data-testid="bubble-image"
      className="max-h-64 max-w-full rounded"
    />
  );
}

/**
 * 一段 dim 色的「思考」块（design §6.1）。
 *
 * 服务端在实时期间只以 `thread/reasoningDelta` 流式推送，session 层已把连续 delta
 * 合并成一块；一旦块级 `item/thinking` 到达，该服务端条目即就地接管同一张卡片。
 * 历史回放期则由 `Item::Thinking` 直接渲染。这里只渲染一块。默认折叠，标题可点开
 * （Mac/iOS 的「默认折叠、可展开」）。`redacted` 块无明文（凭证不下发前端），呈现
 * 「已加密」占位。
 *
 * 折叠状态是**每块**的局部 state：不放进 item，因为 item 归 session 所有、且会被
 * 流式追加就地改写；把 UI 状态塞进去会让「展开」也被序列化进缓存。
 */
function ReasoningBlock({ text, redacted = false }: { text: string; redacted?: boolean }) {
  const [expanded, setExpanded] = useState(false);
  return (
    <div
      data-testid="reasoning-block"
      className="my-1 self-start rounded-md border border-line px-2 py-1 text-xs text-fg-subtle"
    >
      <button
        type="button"
        aria-expanded={expanded}
        onClick={() => setExpanded((v) => !v)}
        className="flex items-center gap-1 font-medium hover:text-fg-muted"
      >
        <span aria-hidden="true">{expanded ? "▾" : "▸"}</span>
        思考
      </button>
      {expanded && (
        <div className="mt-1 whitespace-pre-wrap">
          {/* redacted 块无明文（凭证不下发）：只呈现「已加密」占位，不显示空白。 */}
          {redacted ? "已加密的思考" : text}
        </div>
      )}
    </div>
  );
}

/**
 * `ToolCallCard` memoized on its `item` (and the stable `threadId`/`call`).
 *
 * `Session.apply` replaces the slot with a *new* item object when a tool call
 * starts or completes, so identity is a sound signal here: the same object means
 * the card cannot have changed. Without this boundary every streamed text delta
 * re-rendered every settled tool card (re-serializing its JSON with
 * `JSON.stringify(item.input, null, 2)`), which is unbounded work that grows with
 * the transcript and competes with the sidebar spinner for the main thread.
 *
 * The two image props are deliberately stable — `threadId` is a primitive and
 * `call` is the host's `useCallback([])` seam — so passing them does not weaken
 * the memo. (Do not inline an arrow/object here; that would defeat it.)
 */
const ToolCallRow = memo(ToolCallCard);

/**
 * 未注入 `call` 时的替身（测试/嵌入场景）。**必须是模块级常量**：若在这里写内联
 * 箭头，`ChatItem` 每次渲染都会换新引用，`ToolCallRow` 的 memo 就永远不命中。
 */
const NO_CALL: ImageReadCall = () => Promise.reject(new Error("image/read unavailable"));

/**
 * The text an item renders, lifted out as a primitive.
 *
 * For a prefix item this is the written-once value and it never moves again.
 * For the tail it is re-read on every render from the very object `Session`
 * mutates in place — and it is what `ChatItem`'s comparator compares, and what
 * the agent-message branch renders. The comparator must never read
 * `item.text`; see `ChatItem` for why.
 */
function itemText(item: Item | NoticeItem | ReasoningItem): string {
  switch (item.type) {
    case "agentMessage":
    case "userMessage":
    case "reasoning":
    // `display:"omitted"` 的 thinking 明文是空串，呈现由 `redacted` 决定；text 仍
    // 要照传，好让就地追加能被比较器看见。
    case "thinking":
      return item.text;
    default:
      // `toolCall` 无 text；`notice` 的正文由上面的 notice 分支单独渲染。
      return "";
  }
}

/**
 * One transcript item.
 *
 * `text` is passed as its own primitive prop on purpose. `item/delta` mutates
 * the trailing agent message **in place**, so `prev.item === next.item` always
 * holds; a comparator that read `item.text` would be comparing the current
 * value against itself and the streamed reply would freeze. Comparing the
 * `text` prop instead lets the comparator notice the change while leaving every
 * other message untouched.
 *
 * The tail is handed `itemText(tail)` for every item type, not just an agent
 * message: `thread/reasoningDelta` also appends in place, so a CoT block that
 * is still growing at the tail needs the same treatment (`item.text` alone
 * would compare against itself and the block would stop updating). A settled
 * prefix item never sees a changing `text`, so this costs it nothing.
 *
 * The item-owned branches (notice / user bubble / tool call) read `item`
 * directly: those are replaced object-first, never mutated in place.
 *
 * `threadId` / `call` are the image-read seam (see `App`'s `imageCall`); both are
 * stable, so they do not affect that memoization.
 */
const ChatItem = memo(
  function ChatItem({
    item,
    text,
    threadId,
    call,
  }: {
    item: Item | NoticeItem | ReasoningItem;
    text: string;
    threadId: string | null;
    call: ImageReadCall;
  }) {
    switch (item.type) {
      case "notice":
        return item.config ? (
          <ConfigCard config={item.config.data} error={item.config.error} />
        ) : (
          // 服务端 `thread/notice` 带 level：Warn（通道回退 / effort 降级）用琥珀色
          // 让人一眼看出「走了别的路」；本地 `/命令` 提示与 Info 保持既有中性胶囊。
          <div
            data-testid="notice"
            className={
              item.level === "Warn"
                ? "my-1 self-center rounded-md bg-amber-500/10 px-3 py-1 text-xs text-amber-300"
                : "my-1 self-center rounded-md bg-raised/60 px-3 py-1 text-xs text-fg-muted"
            }
          >
            {item.text}
          </div>
        );
      case "reasoning":
        return <ReasoningBlock text={item.text} />;
      case "thinking":
        return <ReasoningBlock text={item.text} redacted={item.redacted} />;
      case "userMessage":
        return (
          <div className="my-1 max-w-[80%] self-end rounded-lg bg-blue-600 px-3 py-2 text-sm whitespace-pre-wrap text-white">
            {item.attachments && item.attachments.length > 0 && (
              <div className="mb-1 flex flex-wrap gap-1">
                {item.attachments.map((a) => (
                  <span
                    key={a.path}
                    className="rounded bg-blue-700/60 px-1.5 py-0.5 text-xs text-blue-50"
                    title={a.path}
                    data-testid="bubble-attachment"
                  >
                    {a.name || fileNameOf(a.path)}
                  </span>
                ))}
              </div>
            )}
            {item.images && item.images.length > 0 && (
              <div className="mb-1 flex flex-col gap-1">
                {item.images.map((img) => (
                  <BubbleImage key={img.path} path={img.path} threadId={threadId} call={call} />
                ))}
              </div>
            )}
            {item.text}
          </div>
        );
      case "agentMessage":
        return <AgentMessage text={text} />;
      case "toolCall":
        return <ToolCallRow item={item} threadId={threadId} call={call} />;
      default: {
        // Defensive: the server sent an item type this build does not know
        // about. Surface it rather than silently dropping the item.
        const unknown = item as { type?: string };
        return (
          <div className="my-1 font-mono text-xs text-fg-subtle">
            [unsupported item type: {unknown.type ?? "unknown"}]
          </div>
        );
      }
    }
  },
  // Compare the mutable primitive, never `item.text`.
  (prev, next) => prev.item === next.item && prev.text === next.text,
);

/**
 * The live tail: the one item still being appended to in place, rendered with
 * the streaming *agent message's* markdown throttled.
 *
 * **Why this is a separate component and not a hook call inside `ChatView`.**
 * A throttle is stateful, and that state must not outlive the message it
 * belongs to. Called directly in `ChatView`, the hook is shared by whichever
 * item happens to be the tail, so when a *new* message takes the tail over
 * inside the same window, the hook still holds the *previous* message's text
 * and hands it to the new one: the next reply briefly renders as a copy of the
 * last one (observed: the second turn's message showed `answer-one`), and the
 * authoritative text only lands when the window elapses. `ChatView` re-renders
 * between those points, but the hook's `pending`/`shown` are keyed on nothing.
 * Keying this component by the tail's id remounts the throttle whenever the
 * tail changes hands, so it starts from the new message's own text — which is
 * also exactly the correct leading-edge render.
 *
 * Only the `agentMessage` tail is throttled. A non-agent tail (a
 * reasoning/thinking block) keeps flowing through `itemText(item)` unthrottled:
 * its whole text is a single block replaced per delta, throttling it buys
 * nothing, and freezing it would re-break the CoT-tail liveness that `0865bc6a`
 * deliberately fixed. The hook is still *called* unconditionally (rules of
 * hooks) but fed a constant `""` there, so it is inert — no timers, no state
 * churn — for a non-agent tail.
 */
function LiveTail({
  item,
  threadId,
  call,
}: {
  item: Item | NoticeItem | ReasoningItem;
  threadId: string | null;
  call: ImageReadCall;
}) {
  const streamingAgent = item.type === "agentMessage";
  const throttled = useThrottledText(streamingAgent ? item.text : "");
  return (
    <ChatItem
      item={item}
      text={streamingAgent ? throttled : itemText(item)}
      threadId={threadId}
      call={call}
    />
  );
}

/**
 * The settled history above the streaming tail.
 *
 * Its whole purpose is to *not* re-render while only the tail grows. It bails
 * out whenever `items`, `count` and `structuralVersion` are all unchanged; a
 * pure text append touches none of them, so a long session performs no work
 * proportional to its length per delta. (`items` is mutated in place, so the
 * array identity never changes and this memo is what makes the prefix a frozen
 * subtree rather than a re-rendered one.)
 *
 * `count` is explicit because `items` is mutated in place: its length changes
 * without the array identity changing, so the memo cannot rely on the array
 * reference alone to notice that a new item appeared.
 *
 * `structuralVersion` is a **prop, not a `key`**: it only has to make this
 * component re-render *in place*. Keying it would remount the subtree instead
 * and wipe the local state of every child — a CoT the user expanded
 * (`ReasoningBlock`), a tool card they opened (`ToolCallCard`), a half-typed
 * interjection draft — on every `item/started` / `item/completed` / tool event,
 * and would re-issue each prefix bubble's image read. Re-rendering in place is
 * also cheaper: each `ChatItem` still memoizes on its own `item` (and `text`),
 * so only the items that actually moved re-render, whereas a remount re-parses
 * every prefix message's markdown.
 */
const HistoryPrefix = memo(
  function HistoryPrefix({
    items,
    count,
    threadId,
    call,
  }: {
    items: (Item | NoticeItem | ReasoningItem)[];
    count: number;
    threadId: string | null;
    call: ImageReadCall;
    /**
     * Not read by the component body — it exists so the memo comparator below
     * can see the bump and re-render this subtree in place. It is a prop rather
     * than a `key` for the reasons in the doc comment above.
     */
    structuralVersion: number;
  }) {
    const out = [];
    for (let i = 0; i < count; i += 1) {
      const it = items[i];
      out.push(
        <ChatItem
          key={it.id ?? i}
          item={it}
          text={itemText(it)}
          threadId={threadId}
          call={call}
        />,
      );
    }
    return <>{out}</>;
  },
  // `items`/`count` unchanged + no structural change ⇒ nothing above moved.
  (prev, next) =>
    prev.items === next.items &&
    prev.count === next.count &&
    prev.structuralVersion === next.structuralVersion &&
    prev.threadId === next.threadId &&
    prev.call === next.call,
);

/// How each retry cause is worded in the amber retry banner. Kept as a lookup so
/// adding a cause is a one-line change instead of another ternary branch.
const RETRY_CAUSE_LABELS: Record<RetryCause, string> = {
  idle_stall: "Provider stalled",
  request_timeout: "Provider request timed out",
  rate_limited: "Provider rate limited (429)",
  server_error: "Provider server error (5xx)",
  network_error: "Provider unreachable",
};

export function ChatView({
  items,
  error,
  retrying,
  threadId = null,
  call,
  structuralVersion = 0,
}: {
  items: (Item | NoticeItem | ReasoningItem)[];
  error?: string | null;
  retrying?: { attempt: number; max: number; cause: RetryCause } | null;
  /** 当前会话 id，透传给气泡/工具卡读取图片；`null`（无会话）时不读取。 */
  threadId?: string | null;
  /**
   * 宿主注入的图片读取接缝（`App` 的 `imageCall`）。**必须稳定引用**：它进
   * `useImageData` 的依赖数组，每次渲染换新身份会让每张图重新分片拉取。
   */
  call?: ImageReadCall;
  /**
   * Bumped by `Session` when the transcript's *structure* changes (a slot is
   * replaced, an item inserted or removed). Passed into the frozen prefix as a
   * prop so a structural change re-renders it **in place** while a pure text
   * append skips it entirely; a `key` would remount it and wipe child state.
   */
  structuralVersion?: number;
}) {
  const { ref: scrollRef, scrollToBottom } = useStickToBottom();

  // `items` is `session.items` — the same array instance for the whole session,
  // mutated in place (push / items[i] = x). React compares deps with Object.is,
  // so depending on `items` would never re-run this effect when a streamed delta
  // appends text. Depend on values that actually change instead: the item count
  // and the trailing agent text.
  const lastItem = items[items.length - 1];
  const lastText = lastItem && lastItem.type === "agentMessage" ? lastItem.text : "";
  // The live tail is rendered outside the frozen prefix. `tail` is `undefined`
  // for an empty transcript, which is how the prefix's `count` of -1 is guarded.
  // The tail's text is re-read below on every render (via `itemText`): it is the
  // item `Session` appends to in place, so the memo inside `ChatItem` would
  // otherwise never see it move.
  const tail = items.length > 0 ? items[items.length - 1] : undefined;
  const effectiveCall = call ?? NO_CALL;

  // 新内容 / 流式文本变化时贴底。`scrollToBottom` 是稳定引用（useCallback []），
  // 放进依赖数组不会引入额外触发。
  useEffect(() => {
    scrollToBottom();
  }, [items.length, lastText, error, scrollToBottom]);

  return (
    <div
      ref={scrollRef}
      className="flex min-w-0 flex-1 flex-col overflow-x-hidden overflow-y-auto px-4 py-3"
    >
      {/* `structuralVersion` travels as a **prop**, not a `key`. `items` is the
          same array instance for the whole session, so without a changing input
          the memoized prefix would never rebuild after an in-place slot replace.
          A `key` would rebuild it by *remounting* it, which throws away the
          local state underneath — an expanded reasoning block and an open tool
          card — on every item event, and re-issues the
          prefix bubbles' image reads. The prop re-renders in place instead, and
          each `ChatItem` still memoizes, so unchanged messages stay untouched. */}
      <HistoryPrefix
        items={items}
        count={items.length - 1}
        structuralVersion={structuralVersion}
        threadId={threadId}
        call={effectiveCall}
      />
      {tail && (
        <LiveTail
          key={tail.id ?? "tail"}
          item={tail}
          threadId={threadId}
          call={effectiveCall}
        />
      )}
      {retrying && (
        <div className="my-2 rounded-md border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-sm text-amber-300">
          {RETRY_CAUSE_LABELS[retrying.cause]} — retrying {retrying.attempt}/
          {retrying.max}…
        </div>
      )}
      {error && (
        <div className="my-2 rounded-md border border-red-500/40 bg-red-500/10 px-3 py-2 text-sm text-red-300">
          {error}
        </div>
      )}
    </div>
  );
}
