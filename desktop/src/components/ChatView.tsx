import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { RefObject } from "react";
import type { Item, RetryCause } from "../lib/protocol";
import { fileNameOf } from "../lib/attachmentLimits";
import type { NoticeItem, ReasoningItem } from "../lib/session";
import { useImageData, type ImageReadCall } from "../lib/useImageData";
import { useStickToBottom } from "../lib/useStickToBottom";
import { findAnchor, restoreScrollTop, type ItemBox, type ViewAnchor } from "../lib/viewAnchor";
import { useThrottledText } from "../lib/useThrottledText";
import { itemText } from "../lib/itemText";
import { createHeightBook } from "../lib/heightBook";
import { useTranscriptWindow } from "../hooks/useTranscriptWindow";
import type { VisibleRange } from "../lib/transcriptWindow";
import { AgentMessage } from "./MarkdownText";
import { ConfigCard } from "./ConfigCard";
import { HelpCard } from "./HelpCard";
import { UsageCard } from "./UsageCard";
import { ToolCallCard } from "./ToolCallCard";
import { HighlightText } from "./HighlightText";
import { TranscriptPlaceholder } from "./TranscriptPlaceholder";

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
 * 「已加密」占位；明文为空的非 redacted 块呈现「已推理（无明文）」占位。
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
          {/*
            三态：redacted 块无明文（凭证不下发）→「已加密」；有明文 → 原文；
            模型推理过但本次没有可展示的明文 →「已推理（无明文）」。最后一档
            此前会渲染成一片空白。
          */}
          {redacted ? "已加密的思考" : text !== "" ? text : "已推理（无明文）"}
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
    query,
  }: {
    item: Item | NoticeItem | ReasoningItem;
    text: string;
    threadId: string | null;
    call: ImageReadCall;
    /** 搜索关键词。空串表示未在搜索，不产生任何 mark。 */
    query: string;
  }) {
    // 阅读锚点必须挂在实际生成布局盒的节点上：`display: contents` 的矩形恒为
    // 零，采集会把所有消息视为同一位置并错误选中最后一条。用纵向 flex 容器
    // 保留子消息 self-start/self-end 的对齐，禁止收缩以保持可滚动的自然高度。
    const node = (() => {
      switch (item.type) {
        case "notice": {
          // 卡片型 notice 按 `kind` 分派。用**穷尽 switch** 而不是 `?.kind ===` 链：
          // `NoticeBody` 加新 kind 时，下面 `never` 那行会直接编译失败，逼我们同时
          // 更新 `session.CARD_KINDS`，否则搜索层会把卡片当成文本（看得见搜不到）。
          const body = item.body;
          switch (body?.kind) {
            case "config":
              return <ConfigCard config={body.data} error={body.error} />;
            case "help":
              return <HelpCard target={body.target} error={body.error} />;
            case "usage":
              return <UsageCard report={body.report} />;
            default:
              if (body !== undefined) {
                const unhandled: never = body;
                void unhandled;
              }
          }
          // 没有载荷的就是普通文本提示（胶囊）。
          // 服务端 `thread/notice` 带 level：Warn（通道回退 / effort 降级）用琥珀色
          // 让人一眼看出「走了别的路」；本地 `/命令` 提示与 Info 保持既有中性胶囊。
          return (
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
        }
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
            <HighlightText text={item.text} query={query} />
          </div>
        );
      case "agentMessage":
        return <AgentMessage text={text} query={query} />;
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
    })();
    return (
      <div className="flex shrink-0 flex-col" data-item-id={item.id}>
        {node}
      </div>
    );
  },
  // Compare the mutable primitive, never `item.text`.
  (prev, next) =>
    prev.item === next.item && prev.text === next.text && prev.query === next.query,
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
  query,
}: {
  item: Item | NoticeItem | ReasoningItem;
  threadId: string | null;
  call: ImageReadCall;
  query: string;
}) {
  const streamingAgent = item.type === "agentMessage";
  const throttled = useThrottledText(streamingAgent ? item.text : "");
  return (
    <ChatItem
      item={item}
      text={streamingAgent ? throttled : itemText(item)}
      threadId={threadId}
      call={call}
      query={query}
    />
  );
}

/**
 * The mounted slice of settled history, `[start, end)`, above the streaming tail.
 *
 * Its whole purpose is to *not* re-render while only the tail grows. It bails
 * out whenever `items`, the window bounds and `structuralVersion` are all
 * unchanged; a pure text append touches none of them, so a long session performs
 * no work proportional to its length per delta. (`items` is mutated in place, so
 * the array identity never changes and this memo is what makes the window a
 * frozen subtree rather than a re-rendered one.)
 *
 * `start`/`end`/`total` are explicit because `items` is mutated in place: its
 * length changes without the array identity changing, so the memo cannot rely on
 * the array reference alone to notice that a new item appeared.
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
const HistoryWindow = memo(
  function HistoryWindow({
    items,
    total,
    start,
    end,
    threadId,
    call,
    query,
  }: {
    items: (Item | NoticeItem | ReasoningItem)[];
    /** `items.length`。数组原地变更，长度变而身份不变，必须单独传。 */
    total: number;
    /** 挂载区起点（含）。 */
    start: number;
    /** 逻辑终点（**排他**），可等于 `total`（此时含尾部那条）。 */
    end: number;
    threadId: string | null;
    call: ImageReadCall;
    /**
     * Not read by the component body — it exists so the memo comparator below
     * can see the bump and re-render this subtree in place. It is a prop rather
     * than a `key` for the reasons in the doc comment above.
     */
    structuralVersion: number;
    /** 搜索关键词，透传给每条消息。 */
    query: string;
  }) {
    // `end` 是逻辑终点，**含**尾部那条；尾部恒由 `LiveTail` 单独渲染，
    // 所以这里要 `Math.min(end, total - 1)`。漏掉这个 clamp，`end === total` 时
    // 最后一条会被渲染两次（React key 相同，控制台直接报错）。
    const stop = Math.min(end, total - 1);
    const from = Math.min(start, stop);
    const out = [];
    for (let i = from; i < stop; i += 1) {
      const it = items[i];
      // 窗口边界可能落在一次原地替换的中间帧上；`undefined` 直接跳过，
      // 不要渲染出一个没有 id 的空条目。
      if (!it) continue;
      out.push(
        <ChatItem
          key={it.id ?? i}
          item={it}
          text={itemText(it)}
          threadId={threadId}
          call={call}
          query={query}
        />,
      );
    }
    return <>{out}</>;
  },
  // `items`/window bounds unchanged + no structural change ⇒ nothing above moved.
  (prev, next) =>
    prev.items === next.items &&
    prev.total === next.total &&
    prev.start === next.start &&
    prev.end === next.end &&
    prev.structuralVersion === next.structuralVersion &&
    prev.threadId === next.threadId &&
    prev.call === next.call &&
    prev.query === next.query,
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
  textVersion = 0,
  query = "",
  scrollerRef,
  captureViewRef,
  anchor,
  onRememberView,
  syncing = false,
  syncError = null,
  onRetrySync,
  isRemote = false,
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
  /**
   * Bumped by `Session` when **visible text grows in place** (a streamed delta on
   * an agent message, or reasoning text on the open CoT block).
   *
   * 跟随贴底需要「用户看得见的东西变长了」这个信号，而它**不能**从尾部项的
   * 文本推导：服务端会在仍在流式的回答之后插入 CoT 块 / 工具卡（`translate.rs`
   * 的 `ThinkingBlock` / `ToolCall` 两条臂），此时回答被顶到 `length - 2`，
   * 「尾部文本」恒为空，effect 不再重跑，视口就冻在原地——新内容全落在可视区
   * 下方，切走再切回（重挂载强制贴底）才又出现。传 `0` 表示「不跟踪纯文本」，
   * 旁支（服务端只读快照）用这个值。
   */
  textVersion?: number;
  /** 搜索关键词（已防抖）。空串表示未在搜索，不产生任何 mark。 */
  query?: string;
  /**
   * 把内部的滚动容器**借给**宿主（搜索要读它的 scrollTop/clientHeight 判断
   * "当前视口"），与内部 `useStickToBottom` 的 ref 指向同一个元素。
   */
  scrollerRef?: RefObject<HTMLDivElement | null>;
  /** 切换前同步采集，避免最后一个滚动帧被新会话提交取消。 */
  captureViewRef?: RefObject<(() => void) | null>;
  /**
   * 该会话上次离开时的阅读位置；`null` = 离开时贴底，`undefined` = 从没记过。
   * 由宿主按 `threadId` 提供（见 `App` 的 `anchorFor`）。
   */
  anchor?: ViewAnchor | null;
  /**
   * 滚动时上报当前位置。签名带 `threadId` 是为了让"切换前的最后一次采集"仍归属
   * 到旧会话——切换之后 DOM 里已经是新内容，闭包再读当前 id 就错位了。
   */
  onRememberView?: (threadId: string, anchor: ViewAnchor | null) => void;
  /**
   * 这条会话的内容是否仍在从服务端补齐（`ThreadView.syncState === "syncing"`）。
   *
   * 恢复阅读位置必须能区分两种"找不到锚点"：内容**还没到**（回放分帧到达，
   * 中途的帧可能不含锚点那条消息）与锚点**真的没了**（被删/历史被截断）。
   * 前者要继续等，后者才回退到底部。只靠 DOM 猜不出来。
   */
  syncing?: boolean;
  /**
   * 对账失败的原因（`ThreadView.syncError`）。仅 `syncState === "failed"` 时非空。
   */
  syncError?: string | null;
  /**
   * 失败态的「重试」回调。宿主应传 `() => selectThread(id, { forceResume: true })`：
   * `forceResume` 会跳过 `selectThread` 的全部早退，是唯一可靠的强制重同步路径。
   */
  onRetrySync?: () => void;
  /**
   * 是否远端（手机）客户端。**只有远端才渲染骨架屏**——桌面端没有本地缓存、
   * 内容恒来自服务端，加骨架会改变既有观感（设计 §4.3 判定顺序第二条）。
   */
  isRemote?: boolean;
}) {
  const { ref: scrollRef, scrollToBottom, followIfAtBottom, releaseFollow, isFollowing } =
    useStickToBottom();
  // 合并 ref：同一个 DOM 节点交给两方。依赖是两个 ref 本身（稳定引用），
  // 所以不会每次渲染都重设。
  const setScroller = useCallback(
    (el: HTMLDivElement | null) => {
      elRef.current = el;
      scrollRef.current = el;
      if (scrollerRef) scrollerRef.current = el;
    },
    [scrollRef, scrollerRef],
  );

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

  // 未挂载区的高度估算：一条会话只建一本，存活到会话结束。
  const windowBook = useMemo(() => createHeightBook(), []);

  // 测量当前视口。`elRef` 在下面才声明，但闭包只在 rAF 里执行，那时已赋值。
  const measureVisible = useCallback((): VisibleRange | null => {
    const el = elRef.current;
    // 未布局（jsdom、首帧、`scrollHeight` 为 0）时返回 null：`nextWindow` 会保持
    // 跟随且不翻页。这条守卫同时让测试能确定地断言首屏条数。
    if (!el || el.scrollHeight === 0 || el.clientHeight === 0) return null;
    const rows = el.querySelectorAll<HTMLElement>("[data-item-id]");
    if (rows.length === 0) return null;
    const viewTop = el.scrollTop;
    const viewBottom = viewTop + el.clientHeight;
    const firstNode = rows[0];
    const lastNode = rows[rows.length - 1];
    // 相对内容顶部的坐标系：减去容器自身的文档偏移，再加上 scrollTop。
    const base = el.getBoundingClientRect().top - el.scrollTop;
    const firstTop = firstNode.getBoundingClientRect().top - base;
    const lastBottom = lastNode.getBoundingClientRect().bottom - base;
    // `first`/`last` 是**挂载区相对序号**：这一层数不出前面还有多少条，
    // `useTranscriptWindow` 会加上 `window.start` 变成绝对 index。
    return {
      first: 0,
      last: rows.length - 1,
      // **不要夹到 >=0**：`gapTopPx` 为负表示顶边已在视口上方，这是「用户在窗口
      // 内部」的正常状态，必须与「顶边贴住视口顶（0）」区分。夹掉符号会让底部
      // 跟随被误判成翻到顶，窗口逐帧抖动。
      gapTopPx: firstTop - viewTop,
      gapBottomPx: viewBottom - lastBottom,
    };
  }, []);

  // 这里**不传** `focus` prop：锚点恢复（Task 10）与搜索跳转（Task 11）都走
  // 命令式的一次性 `requestFocus()`。传 `focus={ref.current}` 属于渲染期读可变
  // 值，且值恒为 null，等于给 hook 加一条走不通的分支。
  // `requestFocus` 由 Task 10（锚点恢复）与 Task 11（搜索跳转）消费；现在不取，
  // 否则 `noUnusedLocals` 会报错。
  const { window: win, nudge } = useTranscriptWindow({
    itemCount: items.length,
    measure: measureVisible,
    book: windowBook,
  });

  // **翻页必须由滚动驱动。** 滚动既不改变 `itemCount` 也不改变 `focus`，hook 内部
  // 没有任何 React 依赖会因此变化——不在这里接上滚动事件，窗口会永远停在首屏那
  // 60 条，用户滚到顶也加载不出更早的历史。`nudge` 内部按帧去重，可逐帧调用。
  //
  // 与 `useStickToBottom` 各挂一个 `scroll` 监听：职责不同（一个管贴底意图、一个
  // 管窗口翻页），都走 passive，开销可忽略。
  useEffect(() => {
    const el = elRef.current;
    if (!el) return;
    const onScroll = () => nudge();
    el.addEventListener("scroll", onScroll, { passive: true });
    return () => el.removeEventListener("scroll", onScroll);
  }, [nudge]);

  // 把挂载区的真实高度写回 `HeightBook`：中位数越准，spacer 越准，分页后的视觉
  // 跳动越小（设计 §2.4 的螺旋收敛）。
  useEffect(() => {
    const el = elRef.current;
    if (!el) return;
    for (const node of el.querySelectorAll<HTMLElement>("[data-item-id]")) {
      const id = node.dataset.itemId;
      if (!id) continue;
      const h = node.getBoundingClientRect().height;
      // **必须过滤 0**：未布局（首帧、jsdom、刚挂载还没排版）时高度是 0，而
      // `HeightBook.record` 认为 0 合法（折叠条目确实可能为 0）会照收。大量 0
      // 进来会把中位数拉到 0，spacer 归零，滚动条比例整个失真。
      if (h > 0) windowBook.record(id, h);
    }
  }, [win.start, win.end, windowBook]);

  // 采集用的元素句柄：与 `useStickToBottom` 的 ref 指向同一个 DOM 节点。
  const elRef = useRef<HTMLDivElement | null>(null);
  // 回调每次渲染都可能换身份（宿主内联箭头），用 ref 读最新的，避免把采集
  // effect 绑在它的身份上。
  const rememberRef = useRef(onRememberView);
  rememberRef.current = onRememberView;

  // 每条会话只恢复一次：`anchor` 之后可能因为采集而变化，重复对位会把用户
  // 正在做的滚动不断拉回去。
  //
  // **只在真的对好位之后才标记**。切到一条冷会话时，DOM 里还没有任何消息
  // （resume/readItems 尚未回包），此刻若先把 threadId 记下来，等回包后内容
  // 出现时已经"恢复过了"，于是再也补不上——表现为用户被永久留在底部。
  const restoredFor = useRef<string | null | undefined>(undefined);
  /** 已恢复到的位置；漂移校正据此把这一行维持在它当初的偏移上。 */
  const heldAnchor = useRef<ViewAnchor | null>(null);
  /** 用户恢复后自己滚动过 → 位置归他，不再校正。 */
  const userTookOver = useRef(false);
  /** 上一次由**我们**写入的 `scrollTop`；用于识别它自己触发的 scroll 事件。 */
  const selfScrolling = useRef<number | null>(null);
  useLayoutEffect(() => {
    // 恢复状态属于本次访问，而不是上一次成功恢复的会话。
    // A→尚未恢复的 B→A 同样必须重新对位，不能被旧 restoredFor 短路。
    restoredFor.current = undefined;
    heldAnchor.current = null;
    userTookOver.current = false;
    selfScrolling.current = null;
  }, [threadId]);
  useLayoutEffect(() => {
    const el = elRef.current;
    if (!el) return;
    if (restoredFor.current === threadId) return;
    // `null` = 离开时贴底，`undefined` = 从没记过：两者都走既有的"到底"，
    // 由下面的跟随分支负责，这里不重复做。
    if (anchor == null) {
      restoredFor.current = threadId;
      return;
    }
    releaseFollow();
    // 不用 `[data-item-id="..."]` 选择器：id 来自服务端，逗号/引号都会破坏
    // 选择器语法，且 `CSS.escape` 在 jsdom 下不存在。扫描一遍更稳。
    const target = Array.from(el.querySelectorAll<HTMLElement>("[data-item-id]")).find(
      (node) => node.dataset.itemId === anchor.itemId,
    );
    if (!target) {
      // 内容还在补齐：回放分帧到达，此刻可能只收到了前几帧（不含锚点那条）。
      // 不锁定，等内容继续到达再试。
      if (syncing || items.length === 0) return;
      // 已有完整内容却没有这条消息（被删/历史被截断）：回退到底部，别停在
      // 无意义的像素上，并就此锁定——再等也不会出现。
      restoredFor.current = threadId;
      scrollToBottom();
      return;
    }
    const base = el.getBoundingClientRect().top - el.scrollTop;
    const next = restoreScrollTop(
      { id: anchor.itemId, top: target.getBoundingClientRect().top - base },
      anchor,
    );
    if (next === null) {
      restoredFor.current = threadId;
      scrollToBottom();
      return;
    }
    // 锚消息已到达，但其后的回放内容可能还不足以容纳原视口偏移。
    // 此时写入会被夹到临时底部，scroll 采集再把旧锚点抹成 null。
    if (syncing && next > el.scrollHeight - el.clientHeight + 1) return;
    // 与漂移校正共用同一套"这是我们自己写的"标记：恢复这次的写入同样会
    // 触发一个 scroll 事件，不能被读成用户接管。
    // 这是一次新的恢复访问：上一轮停留时用户可能接管过滚动，但不能让该状态
    // 跨会话切换残留，否则 A→B→A 后漂移校正会一直被禁用。
    userTookOver.current = false;
    el.scrollTop = next;
    selfScrolling.current = el.scrollTop;
    restoredFor.current = threadId;
    // 记住"这一行应该在视口顶"，供下方的漂移校正使用。
    heldAnchor.current = anchor;
    // 恢复完立刻放弃跟随，否则下一条 delta 会把人拽回底部。
    releaseFollow();
    // `items.length` 是"可恢复的时机"：冷会话回放是**分帧**到达的，而生产环境下
    // `anchor` 是 Map 里的同一个对象（引用恒定）。只依赖 `anchor` 会让这个 effect
    // 在内容真正出现时不再重跑，恢复永远补不上（实测停在 2424 而非 1200）。
  }, [threadId, anchor, items.length, syncing, scrollToBottom, releaseFollow]);

  // 恢复之后，锚点**上方**的内容还可能改变高度：图片是异步读出来的
  // （`BubbleImage` 在拿到 url 之前返回 null，高度 0），工具卡/推理块也可能
  // 展开。它们一变高，锚点那一行就被整体推下去，用户看到的已经不是离开时的
  // 那一屏——实测偏移 300px。
  //
  // 所以恢复不是"写一次 scrollTop 就完事"，而是：只要用户自己还没滚动，
  // 就一直维持"锚点行顶边落在它当初的偏移上"。用户一滚动即接管，不再干预。
  useEffect(() => {
    const el = elRef.current;
    if (!el) return;
    const held = heldAnchor.current;
    if (!held || restoredFor.current !== threadId) return;

    let frame: number | null = null;
    const realign = () => {
      frame = null;
      const node = elRef.current;
      if (!node || heldAnchor.current !== held) return;
      if (userTookOver.current) return;
      const row = Array.from(node.querySelectorAll<HTMLElement>("[data-item-id]")).find(
        (n) => n.dataset.itemId === held.itemId,
      );
      if (!row) return;
      const base = node.getBoundingClientRect().top - node.scrollTop;
      const next = restoreScrollTop(
        { id: held.itemId, top: row.getBoundingClientRect().top - base },
        held,
      );
      if (next === null) return;
      // 亚像素与行内重排不算漂移，避免无意义地反复写 scrollTop。
      if (Math.abs(node.scrollTop - next) < 1) return;
      // 记下这次是**我们自己**写的：紧随其后的 `scroll` 事件不是用户意图。
      node.scrollTop = next;
      selfScrolling.current = node.scrollTop;
      // scroll 在后续渲染步骤异步派发，不能用 0ms 定时器提前清标记。
      // 由 onUserScroll 消费；用户真滚动到不同位置仍立即接管。
    };
    const schedule = () => {
      if (frame !== null) return;
      frame = requestAnimationFrame(realign);
    };
    // 用户自己的滚动即交还控制权：此后位置归他。
    //
    // **必须排除我们自己写入引起的事件**：真实浏览器在程序化设置 `scrollTop`
    // 之后同样会派发 `scroll`（异步）。若一律当成用户接管，这个校正会在恢复的
    // 那一刻就把自己关掉，锚点上方内容一变高就再也没人管——这正是"偏差有点大"
    // 的直接原因（实测停在 1012，应为 1312）。jsdom 不派发该事件，所以单元测试
    // 曾经全绿。
    const onUserScroll = () => {
      const written = selfScrolling.current;
      selfScrolling.current = null;
      // 位置正是我们刚写下的值 ⇒ 这是自己的回声，不是用户。
      if (written !== null && Math.abs(el.scrollTop - written) < 1) return;
      userTookOver.current = true;
    };
    el.addEventListener("scroll", onUserScroll, { passive: true });

    let observer: ResizeObserver | null = null;
    if (typeof ResizeObserver !== "undefined") {
      observer = new ResizeObserver(schedule);
      // 观察 spacer：它们的高度代表「未挂载区间」的布局变化。未挂载区没有节点
      // 可观察，spacer 是唯一可用的信号。
      for (const sp of el.querySelectorAll("[data-spacer]")) observer.observe(sp);
      // 观察挂载区：图片异步加载、工具卡展开会让已挂载条目变高。
      for (const child of el.querySelectorAll("[data-item-id]")) observer.observe(child);
    }
    // 没有 `ResizeObserver` 的环境（旧 WebView、测试）退化为轮询：只比总高度，
    // 便宜且足以覆盖"图片加载后内容变高"。有观察者时轮询不启动。
    let poll: number | null = null;
    if (!observer) {
      let lastHeight = el.scrollHeight;
      poll = window.setInterval(() => {
        const h = el.scrollHeight;
        if (h === lastHeight) return;
        lastHeight = h;
        schedule();
      }, 100);
    }
    schedule();
    return () => {
      el.removeEventListener("scroll", onUserScroll);
      if (frame !== null) cancelAnimationFrame(frame);
      if (poll !== null) window.clearInterval(poll);
      observer?.disconnect();
    };
  }, [threadId, items.length, syncing]);

  useEffect(() => {
    const el = elRef.current;
    if (!el) return;
    // 采集闭包固定住本次 effect 对应的会话 id（见属性注释）。
    const owner = threadId;
    let frame: number | null = null;

    const capture = () => {
      frame = null;
      const node = elRef.current;
      const report = rememberRef.current;
      // 没有选中会话时没有可归属的位置（也不会有人读它）。
      if (!node || !report || owner === null) return;
      // scroll 的采集帧可能排在 React 已提交新会话 DOM、旧 effect 尚未清理
      // 的窗口中。闭包 owner 只能标记请求归属，不能证明当前 DOM 仍属于它。
      if (node.dataset.threadId !== owner) return;
      // 空/部分回放不是用户离开时的位置，不能覆盖尚待恢复的锚点。
      if (restoredFor.current !== owner || !node.querySelector("[data-item-id]")) return;
      // 用 hook 维护的**相对**意图判断贴底，而不是重算当下的绝对 gap：
      // 采集帧在内容增长之后才跑，此刻 gap 早已超过阈值，重算会把「仍在底部」
      // 误记成「在读历史」，切走再回来就被恢复到消息锚点并放弃跟随。
      if (isFollowing()) {
        report(owner, null);
        return;
      }
      // `getBoundingClientRect().top` 是视口坐标；减去滚动偏移即"内容坐标"。
      const base = node.getBoundingClientRect().top - node.scrollTop;
      const boxes: ItemBox[] = [];
      for (const child of node.querySelectorAll<HTMLElement>("[data-item-id]")) {
        const id = child.dataset.itemId;
        if (!id) continue;
        boxes.push({ id, top: child.getBoundingClientRect().top - base });
      }
      // 一条消息都还没有（刚切过来、resume 尚未回包）：这里没有可归属的位置，
      // 上报 `null` 会被读成「用户看的是最新」，把该会话原有的记录抹掉。
      // 不报，等内容到了自然会再采集一次。
      if (boxes.length === 0) return;
      report(owner, findAnchor(boxes, node.scrollTop));
    };

    const onScroll = () => {
      // 一轮滚动只量一次：读 `getBoundingClientRect` 会强制布局，滚动期间每帧
      // 量 N 条消息会把长会话的滚动拖慢。
      if (frame !== null) return;
      frame = requestAnimationFrame(capture);
    };

    const flushCapture = () => {
      if (frame !== null) cancelAnimationFrame(frame);
      capture();
    };
    if (captureViewRef) captureViewRef.current = flushCapture;
    el.addEventListener("scroll", onScroll, { passive: true });
    capture();
    return () => {
      if (captureViewRef?.current === flushCapture) captureViewRef.current = null;
      el.removeEventListener("scroll", onScroll);
      if (frame !== null) cancelAnimationFrame(frame);
    };
  }, [threadId, captureViewRef]);

  // 新内容 / 流式文本变化时跟随，但**只在用户仍贴底时**。用户上翻历史后，
  // 每个 delta 都强制贴底会让他在整个生成期间反复被拽回底部，根本读不到上面的内容。
  // 仅无阅读记录的首次进入、用户新发消息或新错误横幅强制到底。
  const prevThreadRef = useRef(threadId);
  const prevCountRef = useRef(items.length);
  const prevErrorRef = useRef(error ?? null);
  const didMountRef = useRef(false);
  useEffect(() => {
    // 首次挂载且无阅读记录：打开已有历史的会话落在最新内容上。不能交给
    // `followIfAtBottom`——hook 的 `scroll` 监听此时已把意图读成「不贴底」
    // （新挂载的元素 `scrollTop` 为 0）。
    const firstRun = !didMountRef.current;
    didMountRef.current = true;

    const threadChanged = prevThreadRef.current !== threadId;
    const grew = items.length > prevCountRef.current;
    // 错误横幅是一次性的「必须看到」事件，不是流式增长：保留原来的强制贴底。
    const newError = !threadChanged && !firstRun && error != null && error !== prevErrorRef.current;
    prevThreadRef.current = threadId;
    prevCountRef.current = items.length;
    prevErrorRef.current = error ?? null;

    // 刚发出的用户消息是本地乐观回声（`local-*` id）。它出现在末尾说明是用户
    // 自己按了发送——这条新内容理应可见。
    const newest = items[items.length - 1];
    const sentByUser =
      !threadChanged && !firstRun &&
      grew &&
      newest?.type === "userMessage" &&
      typeof newest.id === "string" &&
      newest.id.startsWith("local-");

    // 恢复锚点的会话不在这里动视口：`useLayoutEffect` 已经对好位，再调
    // `scrollToBottom` 会把它顶掉。
    const restoring = anchor != null && (firstRun || threadChanged || restoredFor.current !== threadId);
    if (restoring) return;
    if (firstRun || sentByUser || newError) scrollToBottom();
    else if (threadChanged) scrollToBottom();
    else followIfAtBottom();
  }, [
    items.length,
    lastText,
    textVersion,
    error,
    threadId,
    anchor,
    syncing,
    scrollToBottom,
    followIfAtBottom,
  ]);

  // 未挂载区间的高度估算。`win.end` 是**排他**终点且含尾部那条，而
  // `HistoryWindow` 只渲染到 `stop = min(end, total - 1)`（尾部恒由 `LiveTail`
  // 渲染）。所以尾部下方未挂载的只有 `[stop, total - 1)` 这一段：
  //   - `end === total`（跟随中）-> `stop === total - 1` -> 下方为 0
  //   - `end < total`          -> `stop === end`         -> 下方 `total - 1 - end` 条
  const medianH = windowBook.median();
  const spacerTop = win.start * medianH;
  const windowStop = Math.min(win.end, Math.max(items.length - 1, 0));
  const spacerBottom = Math.max(0, items.length - 1 - windowStop) * medianH;

  return (
    <div
      ref={setScroller}
      data-thread-id={threadId ?? undefined}
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
      {/* 转录为空时才谈得上「占位」。有内容一律原样，绝不遮挡用户正在读的东西。
          桌面端（`!isRemote`）永不渲染：不加任何新视觉元素（设计 §6.1）。 */}
      {items.length === 0 && isRemote && (syncing || syncError !== null) ? (
        syncError !== null ? (
          <TranscriptPlaceholder kind="failed" reason={syncError} onRetry={onRetrySync} />
        ) : (
          <TranscriptPlaceholder kind="loading" />
        )
      ) : null}
      {/* 未挂载历史的占位高度：滚动条比例因此真实，滚动仍由浏览器原生完成。 */}
      <div data-spacer="top" aria-hidden="true" style={{ height: spacerTop }} />
      <HistoryWindow
        items={items}
        total={items.length}
        start={win.start}
        end={win.end}
        structuralVersion={structuralVersion}
        threadId={threadId}
        call={effectiveCall}
        query={query}
      />
      {tail && (
        <LiveTail
          key={tail.id ?? "tail"}
          item={tail}
          threadId={threadId}
          call={effectiveCall}
          query={query}
        />
      )}
      <div data-spacer="bottom" aria-hidden="true" style={{ height: spacerBottom }} />
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
