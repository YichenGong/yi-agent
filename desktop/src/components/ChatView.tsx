import { memo, useEffect, useState } from "react";
import type { Item, RetryCause } from "../lib/protocol";
import { fileNameOf } from "../lib/attachmentLimits";
import type { NoticeItem, ReasoningItem } from "../lib/session";
import { useImageData, type ImageReadCall } from "../lib/useImageData";
import { useStickToBottom } from "../lib/useStickToBottom";
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
 * One transcript item.
 *
 * The memo boundary sits on the *leaf* components and their value props, not on
 * the item object: `item/delta` mutates the trailing agent message **in place**,
 * so a memo keyed on object identity would never see the text change and the
 * streamed reply would freeze. Passing `text` (a primitive) lets React's default
 * shallow prop comparison notice the change while leaving every other message —
 * whose text did not move — untouched.
 *
 * `threadId` / `call` are the image-read seam (see `App`'s `imageCall`); both are
 * stable, so they do not affect that memoization.
 */
function ChatItem({
  item,
  threadId,
  call,
}: {
  item: Item | NoticeItem | ReasoningItem;
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
      return <AgentMessage text={item.text} />;
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
}

export function ChatView({
  items,
  error,
  retrying,
  threadId = null,
  call,
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
}) {
  const { ref: scrollRef, scrollToBottom } = useStickToBottom();

  // `items` is `session.items` — the same array instance for the whole session,
  // mutated in place (push / items[i] = x). React compares deps with Object.is,
  // so depending on `items` would never re-run this effect when a streamed delta
  // appends text. Depend on values that actually change instead: the item count
  // and the trailing agent text.
  const lastItem = items[items.length - 1];
  const lastText = lastItem && lastItem.type === "agentMessage" ? lastItem.text : "";
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
      {items.map((item, index) => (
        // `index` is only a fallback key for defensive unknown-type items; every
        // known item carries a stable protocol `id`.
        <ChatItem
          key={item.id ?? index}
          item={item}
          threadId={threadId}
          call={effectiveCall}
        />
      ))}
      {retrying && (
        <div className="my-2 rounded-md border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-sm text-amber-300">
          {retrying.cause === "request_timeout"
            ? "Provider request timed out"
            : "Provider stalled"}{" "}
          — retrying {retrying.attempt}/{retrying.max}…
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
