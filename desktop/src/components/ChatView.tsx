import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { RefObject } from "react";
import type { Item, RetryCause } from "../lib/protocol";
import { fileNameOf } from "../lib/attachmentLimits";
import type { NoticeItem, ReasoningItem } from "../lib/session";
import { useImageData, type ImageReadCall } from "../lib/useImageData";
import { useTranscriptScroll } from "../hooks/useTranscriptScroll";
import { measureTranscript, type WindowMeasurement } from "../lib/transcriptGeometry";
import { findAnchor, type ItemBox, type ViewAnchor } from "../lib/viewAnchor";
import { useThrottledText } from "../lib/useThrottledText";
import { itemText } from "../lib/itemText";
import { createHeightBook } from "../lib/heightBook";
import { useTranscriptWindow } from "../hooks/useTranscriptWindow";
import { DEFAULT_LIMITS } from "../lib/transcriptWindow";
import type { TranscriptWindowApi } from "../hooks/useTranscriptWindow";
import { AgentMessage } from "./MarkdownText";
import { ConfigCard } from "./ConfigCard";
import { HelpCard } from "./HelpCard";
import { UsageCard } from "./UsageCard";
import { ToolCallCard } from "./ToolCallCard";
import { HighlightText } from "./HighlightText";
import { SEARCH_HIT_SELECTOR } from "../hooks/useTranscriptSearch";
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
    index = -1,
    region = "history",
  }: {
    item: Item | NoticeItem | ReasoningItem;
    text: string;
    threadId: string | null;
    call: ImageReadCall;
    /** 搜索关键词。空串表示未在搜索，不产生任何 mark。 */
    query: string;
    /**
     * 该条在 `items` 里的**绝对** index。测量层直接读 `data-item-index`，
     * 不再让 hook 用 `window.start + 相对序号` 换算（tail 不在历史窗口内，
     * 那样换算会把历史边界比错）。
     */
    index?: number;
    /** 历史区还是恒挂的 live tail。历史 gap 只由 `history` 行计算。 */
    region?: "history" | "tail";
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
      <div
        className="flex shrink-0 flex-col"
        data-item-id={item.id}
        data-item-index={index}
        data-transcript-region={region}
      >
        {node}
      </div>
    );
  },
  // Compare the mutable primitive, never `item.text`.
  //
  // `index`/`region` 必须比较：它们决定 `data-item-index` / `data-transcript-region`，
  // 而测量层（`transcriptGeometry`）按这两个属性把 DOM 行映射回绝对序号与所属
  // 区域。前面插入一条消息会让后面所有行的绝对 index 平移；不重渲染就留着旧值，
  // 测量层会认为「这几行属于别的区间」，窗口化与锚点恢复随之错位。
  (prev, next) =>
    prev.item === next.item &&
    prev.text === next.text &&
    prev.query === next.query &&
    prev.index === next.index &&
    prev.region === next.region,
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
  index,
  threadId,
  call,
  query,
}: {
  item: Item | NoticeItem | ReasoningItem;
  /** 该条在 `items` 里的绝对 index（tail 恒为 `items.length - 1`）。 */
  index: number;
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
      index={index}
      region="tail"
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
          index={i}
          region="history"
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
  windowApiRef,
  activeHit,
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
  /**
   * 可选的出口：把窗口 API 交给宿主，供搜索跳转等命令式开窗使用。
   *
   * 用 ref 而不是 props 回调：`requestFocus` 的身份必须稳定，且宿主要在
   * **事件处理器里**调用它，不能靠 props 变化驱动。
   */
  windowApiRef?: { current: TranscriptWindowApi | null };
  /**
   * 搜索的当前命中。命中多半未挂载（窗口化），所以这里只负责**给已挂载的
   * 那个 `mark` 打标**；未挂载时由宿主先开窗，挂载后再生效。
   */
  activeHit?: { itemId: string; offset: number } | null;
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
  // 布局归属 = **只有会话身份**。换会话时上一条的位置不属于这一条（继承它正是
  // 「切到等长会话却停在中段」的成因），所以必须重建协调器。
  //
  // 但**不能把内容版本放进归属**：`textVersion` 每个 token 都变，`structuralVersion`
  // 每次追加 item 也变，而协调器重建会把 `followingIntent` 复位成 true、把阅读
  // 锚点丢掉。用户上翻历史后只要还有东西流进来，每个 delta 都会被重新判定为
  // 「在跟随」并拽回底部——实测本用例落在贴底 13224 而不是锚点纠正后的 5300。
  // 内容增长要的是「重新测量」，那是 `planLayout` 的 `generation`，不是新归属。
  const layoutOwnerKey = threadId ?? "none";
  // 布局变化（图片迟到、工具卡展开、窗口旋转、渲染后 spacing 收敛）必须回到同一个
  // 决策者。交给 `planLayout` 是因为它内部就是「读几何 → 协调器决策 → 唯一写入」；
  // 直接回调 React setState 只会引入新的一帧而不解决位置。
  // `onLayoutChange` 用 ref 转达（hook 内部按 ref 读），所以这里传一个稳定函数。
  const layoutTickRef = useRef<() => void>(() => {});
  const scrollApi = useTranscriptScroll({
    ownerKey: layoutOwnerKey,
    onLayoutChange: () => layoutTickRef.current(),
  });
  const {
    ref: attachScroller,
    scrollToBottom,
    followIfAtBottom,
    releaseFollow,
    isFollowing,
    requestRestore,
    holdAnchor,
    planLayout,
    hasPendingRestore,
  } = scrollApi;
  // 合并 ref：同一个 DOM 节点交给两方。依赖是两个 ref 本身（稳定引用），
  // 所以不会每次渲染都重设。
  const setScroller = useCallback(
    (el: HTMLDivElement | null) => {
      elRef.current = el;
      attachScroller(el);
      if (scrollerRef) scrollerRef.current = el;
    },
    [attachScroller, scrollerRef],
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

  // 未挂载区的高度估算（Spec §4.4 第 1 条）。
  //
  // 账本属于**布局归属**而不是组件实例：上一条会话的消息高度对这条毫无意义，
  // 沿用就会让短会话继承长会话的中位数，切过去的第一屏 spacer 按错误高度预留，
  // 滚动条比例与跳转位置一起失真（实测换会话后 spacer 仍是 5600px）。
  // `layoutOwnerKey` 变化即换新账本；旧的那本随闭包被回收，不做全局缓存。
  const windowBook = useMemo(() => createHeightBook(), [layoutOwnerKey]);

  // 测量当前视口。`elRef` 在下面才声明，但闭包只在 rAF 里执行，那时已赋值。
  // 测量当前视口。`elRef` 在下面才声明，但闭包只在 rAF 里执行，那时已赋值。
  //
  // 两条口径都在 `measureTranscript` 里钉住：绝对 index 直接读 DOM 属性
  // （`tail` 不在历史窗口内，旧实现 `window.start + 相对序号` 会把边界比错），
  // 历史 gap 只由历史区行计算（恒挂的 LiveTail 会把首末挂载行撑到视口内，
  // 读成「已覆盖」后不再开窗，必须切走再切回才补上）。
  const measureVisible = useCallback((): WindowMeasurement | null => {
    const el = elRef.current;
    // 未布局（jsdom、首帧、`scrollHeight` 为 0）时返回 null：`nextWindow` 会保持
    // 跟随且不翻页。这条守卫同时让测试能确定地断言首屏条数。
    if (!el || el.scrollHeight === 0) return null;
    return measureTranscript(el);
  }, []);

  // 这里**不传** `focus` prop：锚点恢复（Task 10）与搜索跳转（Task 11）都走
  // 命令式的一次性 `requestFocus()`。传 `focus={ref.current}` 属于渲染期读可变
  // 值，且值恒为 null，等于给 hook 加一条走不通的分支。
  // `requestFocus` 由 Task 10（锚点恢复）与 Task 11（搜索跳转）消费；现在不取，
  // 否则 `noUnusedLocals` 会报错。
  // 条目 id 顺序：视口落进未挂载区间时，用它把 `scrollTop` 换算成条号并发一次
  // focus。依赖 `items.length` 而不是 `items`：`items` 是同一个数组实例原地变化，
  // 按引用比较永远不会更新；长度变化已经覆盖「结构性增删」，而纯文本增长不影响
  // 条号对应关系。
  const anchorIds = useMemo(
    () => items.map((it) => it.id ?? ""),
    // `items` 是同一个数组实例原地变化（`Session.apply` 直接改它），按引用做依赖
    // 等于永不更新；`items.length` 覆盖结构性增删，`structuralVersion` 覆盖「同长度
    // 换槽」。纯文本增长不改变 id 顺序，不需要重建。
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [items.length, structuralVersion, threadId],
  );
  const windowApi = useTranscriptWindow({
    itemCount: items.length,
    threadId,
    measure: measureVisible,
    book: windowBook,
    following: isFollowing,
    anchorIds,
  });
  const { window: win, nudge, requestFocus } = windowApi;

  // 宿主拿到的必须是**稳定**对象：它在事件处理器里调用 `requestFocus`，
  // 每次渲染换新引用会让调用方每次都得重取。
  useLayoutEffect(() => {
    if (!windowApiRef) return;
    windowApiRef.current = windowApi;
    return () => {
      windowApiRef.current = null;
    };
  }, [windowApiRef, windowApi]);

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

  // 给「当前命中」的那一个 `mark` 打标，CSS 靠它把当前命中画得比其余更实。
  // 必须在渲染落地后做：`mark` 由 rehype 插件产出，随 query 变化被整体替换，
  // 存在 state 里的节点引用必然失效，所以每轮都重新找。
  useEffect(() => {
    const el = elRef.current;
    if (!el) return;
    const previous = Array.from(el.querySelectorAll<HTMLElement>("[data-search-active]"));
    for (const node of previous) node.removeAttribute("data-search-active");
    if (!activeHit) return;
    // 只在该条已挂载时才有节点。条内第 offset 个命中 = 该行里的第 offset 个 mark。
    // 不用属性选择器：id 来自服务端，引号/逗号会破坏语法，且 `CSS.escape`
    // 在 jsdom 下不存在。扫描一遍更稳（与锚点恢复的做法一致）。
    const row = Array.from(el.querySelectorAll<HTMLElement>("[data-item-id]")).find(
      (n) => n.dataset.itemId === activeHit.itemId,
    );
    if (!row) return;
    const marks = row.querySelectorAll<HTMLElement>(SEARCH_HIT_SELECTOR);
    marks[activeHit.offset]?.setAttribute("data-search-active", "true");
  }, [activeHit, query, win.start, win.end]);

  // 窗口变化会改变内容高度，而 WKWebView **不实现 scroll anchoring**：视口上方多挂
  // 200 条，用户看到的内容就整体位移；从下方卸载则让 `scrollHeight` 变小、
  // `scrollTop` 被浏览器往下夹，`useStickToBottom` 会把这次位移读成「用户上翻」并
  // 永久关闭跟随。所以每次窗口变更都要补偿一次。
  // **每次布局提交只允许一个位置写入者。** 旧实现有五个独立写入者（贴底、
  // 整体高度差补偿、锚点恢复、漂移校正、窗口 focus）互相覆盖：补偿把
  // `scrollHeight` 差全部当成「上方新增」，锚点下方长高时就把读者推走。
  // 现在统一交给 `TranscriptScrollCoordinator`：它按「pending 恢复 > 阅读锚点 >
  // 贴底」排序，一次布局最多写一次 `scrollTop`。
  //
  // 依赖里**没有** `win.start`/`win.end`：中位数收敛、图片迟到、窗口 trim 都会
  // 改变几何，只在窗口区间变化时维护上一份几何，会把首次收敛的高度差一路攒到
  // 用户下次滚动（Spec §4.4 第 3 条）。
  useLayoutEffect(() => {
    // 先记账、再写 spacer、最后决策：本次布局测到的真实高度就是这一帧的位置依据。
    // 顺序不能换——先 `planLayout` 会把「还没校正的 scrollHeight」当成结论。
    const measured = measureMountedHeights();
    const top = spacerTopRef.current;
    const bottom = spacerBottomRef.current;
    if (top) top.style.height = `${win.start * measured}px`;
    if (bottom) bottom.style.height = `${bottomSpacerCount * measured}px`;
    // 只在 spacer 可见（即上下真的还有未挂载区间）时才让 React 记住新中位数：
    // 跟随状态下两个 spacer 都是 0，高度与中位数无关，此刻 setState 只会凭空多
    // 一次渲染去和贴底写入抢帧。
    if (spacerVisible && measured !== spacerMedian) setSpacerMedian(measured);
    planLayout();
  });

  // 尺寸变化的唯一落点：仍由协调器决定写不写、写哪里。
  layoutTickRef.current = () => {
    planLayout();
  };

  /**
   * 把挂载区的真实高度写回 `HeightBook`（Spec §4.4 第 2、3 条）。
   *
   * **每次布局提交都要做**，不能只在 `win.start/end` 变化时更新：图片迟到、首屏
   * 测量落定、工具卡展开都不改变窗口区间，只在窗口变化时记账会让首次收敛出来的
   * 中位数一直攒到用户滚动——表现就是「刚进会话高度不对，动一下才正常」。
   *
   * 但**不能因此每帧 setState**：它与「跟随贴底」的写入交错会让视口停在中间
   * （实测流式时 gap 280px 不再贴底）。所以返回「中位数是否真的变了」，只在变了
   * 时让调用方触发一次渲染；未变化时纯记账，不进入 React 更新。
   */
  const measureMountedHeights = useCallback((): number => {
    const el = elRef.current;
    if (!el) return windowBook.median();
    for (const node of el.querySelectorAll<HTMLElement>("[data-item-id]")) {
      const id = node.dataset.itemId;
      if (!id) continue;
      // 零高与非有限值由 `HeightBook.record` 拒绝入账（见其接口注释）。
      windowBook.record(id, node.getBoundingClientRect().height);
    }
    return windowBook.median();
  }, [windowBook]);

  /**
   * 已渲染 spacer 所用的中位数。
   *
   * 它是**渲染输入**（spacer 高度 = 条数 × 中位数），所以必须能触发渲染；但每帧
   * 都 setState 会让「跟随贴底」的写入与这次渲染抢帧，实测流式时视口停在中间
   * （gap 280px 而不是 0）。因此：
   *   - 渲染期读 state，保证 React 重渲染时 spacer 不会回退到旧值；
   *   - 每次布局提交**直接把最新高度写进 DOM**，不等下一次渲染。
   * 布局 effect 在绘制前执行，所以即使中间有一次渲染把高度写成旧值，也会在
   * 同一帧内被纠正，用户看不到跳动。
   */
  const [spacerMedian, setSpacerMedian] = useState(() => windowBook.median());
  // 换 owner 会换一本**新的**账本，它的中位数从 fallback 重新开始；旧值留着会让
  // spacer 停在旧会话的高度估算上。只在换 owner 时走一次渲染，之后靠布局期直写。
  useLayoutEffect(() => {
    setSpacerMedian(windowBook.median());
  }, [layoutOwnerKey, windowBook]);

  // 采集用的元素句柄：与 `useStickToBottom` 的 ref 指向同一个 DOM 节点。
  const elRef = useRef<HTMLDivElement | null>(null);
  // 回调每次渲染都可能换身份（宿主内联箭头），用 ref 读最新的，避免把采集
  // effect 绑在它的身份上。
  const rememberRef = useRef(onRememberView);
  rememberRef.current = onRememberView;
  // 采集 effect 只依赖 threadId（见下文），holdAnchor 的引用要经 ref 转达，
  // 免得把它塞进依赖后每次渲染都重建监听。
  const holdAnchorRef = useRef(holdAnchor);
  holdAnchorRef.current = holdAnchor;

  // 每条会话只恢复一次：`anchor` 之后可能因为采集而变化，重复对位会把用户
  // 正在做的滚动不断拉回去。
  //
  // **只在真的对好位之后才标记**。切到一条冷会话时，DOM 里还没有任何消息
  // （resume/readItems 尚未回包），此刻若先把 threadId 记下来，等回包后内容
  // 出现时已经"恢复过了"，于是再也补不上——表现为用户被永久留在底部。
  const restoredFor = useRef<string | null | undefined>(undefined);
  /** 已恢复到的位置；漂移校正据此把这一行维持在它当初的偏移上。 */
  const heldAnchor = useRef<ViewAnchor | null>(null);
  /** 已在数据层找到 index、等 `requestFocus` 开的那一窗渲染落地的锚点。 */
  const pendingAnchorRef = useRef<ViewAnchor | null>(null);
  useLayoutEffect(() => {
    // 恢复状态属于本次访问，而不是上一次成功恢复的会话。
    // A→尚未恢复的 B→A 同样必须重新对位，不能被旧 restoredFor 短路。
    restoredFor.current = undefined;
    heldAnchor.current = null;
    pendingAnchorRef.current = null;
  }, [threadId]);
  useLayoutEffect(() => {
    if (restoredFor.current === threadId) return;
    // `null` = 离开时贴底，`undefined` = 从没记过：两者都走既有的"到底"，
    // 由下面的跟随分支负责，这里不重复做。
    if (anchor == null) {
      restoredFor.current = threadId;
      return;
    }

    // 第一步：在**数据层**找 index。不能按 DOM 找——窗口化后锚点通常不在挂载区，
    // 按 DOM 找不到会被误判成「锚点真的没了」而静默跳到底部，这正是要消除的退化。
    const index = items.findIndex((it) => it.id === anchor.itemId);
    if (index === -1) {
      // 内容还在补齐：回放分帧到达，此刻可能只收到了前几帧（不含锚点那条）。
      // 不锁定，等内容继续到达再试。
      if (syncing || items.length === 0) return;
      // 已有完整内容却没有这条消息（被删/历史被截断）：回退到底部，别停在
      // 无意义的像素上，并就此锁定——再等也不会出现。
      restoredFor.current = threadId;
      scrollToBottom();
      return;
    }

    // 第二步：开窗把锚点包进挂载区。用 limits 表达，不写字面量。
    requestFocus({ index, before: DEFAULT_LIMITS.buffer, after: 3 * DEFAULT_LIMITS.buffer });
    releaseFollow();
    // 第三步：**不在这里写 scrollTop**。锚点此刻还没挂载，`querySelector` 仍找
    // 不到它；等下面的「锚点已挂载」effect 在对的位置接手。
    pendingAnchorRef.current = anchor;
    // `items.length` 是"可恢复的时机"：冷会话回放是**分帧**到达的，而生产环境下
    // `anchor` 是 Map 里的同一个对象（引用恒定）。只依赖 `anchor` 会让这个 effect
    // 在内容真正出现时不再重跑，恢复永远补不上（实测停在 2424 而非 1200）。
  }, [threadId, anchor, items.length, syncing, scrollToBottom, releaseFollow, items, requestFocus]);

  // 锚点进入挂载区之后才由协调器对位：`planLayout` 会按 pending 目标行的真实
  // 位置算写入点，内容不足以容纳原偏移时保持 pending（不标已恢复）。
  //
  // 这里**不再自己写 scrollTop**：位置写入只剩 `planLayout` 一条路径。
  // 只要窗口/内容变化就再继续推进一次；恢复完成由 `restoredFor` 记账。
  useEffect(() => {
    if (restoredFor.current === threadId) return;
    const pending = pendingAnchorRef.current;
    if (!pending) return;
    requestRestore(pending);
    planLayout();
    const el = elRef.current;
    // 已经对好位（协调器消费了 pending）就结算本次恢复，并记住这条锚点供
    // 后续「上方增高」纠正使用 —— 否则锚点上方图片迟到时视口会整体位移。
    if (el && !hasPendingRestore()) {
      restoredFor.current = threadId;
      heldAnchor.current = pending;
      pendingAnchorRef.current = null;
      // 恢复完立刻放弃跟随，否则下一条 delta 会把人拽回底部。
      releaseFollow();
    }
  }, [
    threadId,
    win.start,
    win.end,
    items.length,
    syncing,
    requestRestore,
    planLayout,
    releaseFollow,
    hasPendingRestore,
  ]);

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
      const next = findAnchor(boxes, node.scrollTop);
      report(owner, next);
      // 用户自己滚到的新位置就是新的阅读锚点：此后锚点**上方**内容长高
      // （图片迟到、工具卡展开）必须把它纠正回来，否则视口会整体位移。
      // 贴底时不持锚点——那是「跟随」而不是「阅读中某一屏」。
      if (next !== null) holdAnchorRef.current?.(next);
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
    // spacer 的中位数一变，`scrollHeight` 就在**这次提交**里变了：内容变高而
    // 视口没动，等于把贴底位置甩到下面（实测流式时 gap 280px）。跟随必须跟着
    // 这次高度变化再跑一遍，否则「内容增长」与「高度校正」成为两个互不相干的
    // 渲染周期，后者会把前者的贴底成果冲掉。
    spacerMedian,
  ]);

  // 未挂载区间的高度估算。`win.end` 是**排他**终点且含尾部那条，而
  // `HistoryWindow` 只渲染到 `stop = min(end, total - 1)`（尾部恒由 `LiveTail`
  // 渲染）。所以尾部下方未挂载的只有 `[stop, total - 1)` 这一段：
  //   - `end === total`（跟随中）-> `stop === total - 1` -> 下方为 0
  //   - `end < total`          -> `stop === end`         -> 下方 `total - 1 - end` 条
  // `heightRevision` 是本渲染的依赖：账本每次真实变化都会换一个值，spacer 据此
  // 在**同一个布局周期**里跟上新的中位数，而不是等下一次窗口移动。
  const medianH = spacerMedian;
  const spacerTop = win.start * medianH;
  const windowStop = Math.min(win.end, Math.max(items.length - 1, 0));
  const bottomSpacerCount = Math.max(0, items.length - 1 - windowStop);
  const spacerBottom = bottomSpacerCount * medianH;
  // 「spacer 是否真的占位」决定中位数变化要不要触发渲染（见上面记账 effect）。
  const spacerVisible = spacerTop > 0 || spacerBottom > 0;
  // 布局期直写 spacer 高度用的句柄，与上面两个数量配合。
  const spacerTopRef = useRef<HTMLDivElement | null>(null);
  const spacerBottomRef = useRef<HTMLDivElement | null>(null);

  return (
    <div
      ref={setScroller}
      data-thread-id={threadId ?? undefined}
      // 稳定的「这是转录滚动容器」标记：设计/验收脚本按它取元素（`data-thread-id`
      // 只表达归属，换会话就变，且旁支的合成 id 也不适合当选择器）。
      data-transcript-scroller=""
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
      {/* 未挂载历史的占位高度：滚动条比例因此真实，滚动仍由浏览器原生完成。
          **必须 `shrink-0`**：容器是 `flex-col`，flex 子项默认 `flex-shrink: 1`，
          而 spacer 没有内容、`min-height: auto` 也解析成 0，于是它会被压成 0 高度
          ——占位完全失效，`scrollHeight` 只剩已挂载部分，滚动条比例失真，滚到
          未挂载区间就是一片空白（真实浏览器实测：样式 53984px，实际 0px）。 */}
      <div
        ref={spacerTopRef}
        data-spacer="top"
        aria-hidden="true"
        className="shrink-0"
        style={{ height: spacerTop }}
      />
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
          index={items.length - 1}
          threadId={threadId}
          call={effectiveCall}
          query={query}
        />
      )}
      <div
        ref={spacerBottomRef}
        data-spacer="bottom"
        aria-hidden="true"
        className="shrink-0"
        style={{ height: spacerBottom }}
      />
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
