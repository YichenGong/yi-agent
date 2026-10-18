import type { Attachment, Item, Notification, RetryCause, TurnStatus, Usage } from "./protocol";

/**
 * `Notification` 的 `turn/retry` 载荷里 `cause` 的类型是 `string`——那就是线形状
 * （`protocol.rs` 发的是普通 `String`，值为 `"idle_stall"` 等）。但 `retrying.cause`
 * 是前端自己的窄联合 `RetryCause`，而 `ChatView` 的 `RETRY_CAUSE_LABELS` 要拿它查表。
 *
 * 所以在这个赋值边界上做一次**真实的**检查（而不是 `as RetryCause` 那种只让编译器
 * 闭眼的断言）：认得的原因原样透传；认不得的（服务端将来新增原因、前端还没跟上）
 * 归到 `network_error`，其文案「Provider unreachable」对任何非特定的重试都成立。
 * 这样最坏情况是文案退化，而不是 `RETRY_CAUSE_LABELS[undefined]` 静默渲染成空标签。
 */
const RETRY_CAUSES: readonly RetryCause[] = [
  "idle_stall",
  "request_timeout",
  "rate_limited",
  "server_error",
  "network_error",
];

function toRetryCause(cause: string): RetryCause {
  return RETRY_CAUSES.includes(cause as RetryCause) ? (cause as RetryCause) : "network_error";
}

let localSeq = 0;
const nextLocalId = () => `local-${++localSeq}`;

/**
 * A slash-command output line. Not part of the wire protocol: the app-server
 * knows nothing about it. It mirrors the TUI's `HistoryCell::Separator`, which
 * is how the TUI renders command output without pretending the agent said it.
 */
export interface NoticeItem {
  type: "notice";
  id: string;
  text: string;
  /**
   * 服务端 `thread/notice`（通道回退 / effort 降级）的级别，用来给 Warn 变体换色调。
   * 本地 `/命令` 提示没有级别（缺省），渲染成既有的中性胶囊。**PascalCase**：照抄
   * 线形状（core 的 `NoticeLevel` 无 `rename_all`）。
   */
  level?: "Info" | "Warn";
  /**
   * 结构化载荷。`/config` 用它携带 `config/read` 的原始结果（或读取错误），
   * 由 `ChatView` 渲染成一张配置卡片。缺省即普通文本 notice。
   */
  config?: { data?: Record<string, unknown> | null; error?: string | null };
}

/**
 * 一段累积的思维链（CoT）。**本地展示条目，不是 wire `Item`**：服务端在实时期间
 * 只以 `thread/reasoningDelta` 流式推送，好让前端保留流式手感；一旦块级
 * `item/thinking`（服务端权威条目）到达，本块即被**就地接管**（见 `apply`）。
 * 回放期则直接由 `Item::Thinking` 驱动，本类型不出场。它同样绝不回灌进后续轮次。
 *
 * 连续 delta 合并进**同一个**块——与 TUI 的单 `HistoryCell::Reasoning` 同一契约，
 * 否则一个 token 一个块会把转录刷屏。
 */
export interface ReasoningItem {
  type: "reasoning";
  id: string;
  text: string;
}

/**
 * Accumulates the client-side view of one app-server thread.
 *
 * `apply` folds each incoming notification into `items` / turn / usage state.
 * Items are keyed by their protocol `id`, so `item/completed` replaces the
 * placeholder created by `item/started`, and `item/delta` appends streamed
 * agent text to the existing item in place.
 */
export class Session {
  /**
   * 服务端未能消费、已回推的追加消息 id。空数组表示没有待恢复的文本。
   *
   * 只存 id：文本仍留在服务端的请求记录与本地输入框草稿里，回推的语义是
   * "这次追加没生效，请把它还给用户"。渲染层据此提示用户重新提交。
   */
  returnedInterjections: string[] = [];
  items: (Item | NoticeItem | ReasoningItem)[] = [];
  turnActive = false;
  lastStatus: TurnStatus | null = null;
  lastError: string | null = null;
  usage: Usage | null = null;
  retrying: { attempt: number; max: number; cause: RetryCause } | null = null;

  /**
   * 仍处于「流式累积」中的那个推理块，或 `null`（没有打开的块）。
   *
   * 连续 delta 合并进它；一旦有任何**服务端项**落到它之后（本轮开项的 agentMessage、
   * 下一轮的 userMessage、工具卡……），运行即被打断——见 {@link breakReasoningRun}。
   * 不能用「数组末尾是不是推理块」来判断：服务端项经
   * {@link insertAfterLastServerItem} 插在最后一个服务端项之后，会把推理块甩到
   * 它自己的答案后面。
   */
  private openReasoning: ReasoningItem | null = null;

  /**
   * 服务端 item 流里最新的、带服务端 id 的 item id（本地生成的用户气泡/通知
   * 不计入）。它同时是"续读游标"：远程客户端从冷会话返回时，用 `thread/readItems`
   * 带 `afterItemId` 只补齐这之后的内容。`null` 表示还没有任何服务端 item。
   */
  lastServerItemId: string | null = null;

  /**
   * The server's content generation for this thread, as last learned from
   * `syncPlan` / `ThreadSummary`. `null` means we have not learned one yet
   * (first sighting this session) — which must NOT be treated as "matches"
   * (spec §4.3): a missing version degrades to the conservative full resume.
   */
  contentEpoch: number | null = null;

  /**
   * True once a full replay was triggered by a generation change. Guards
   * `prefillFromCache`: an async cache read that resolves *after*
   * `adoptEpoch` wiped the transcript would otherwise see `items.length === 0`
   * and inject the very stale content we just judged invalid (spec §6.4).
   */
  epochInvalidated = false;

  /**
   * Server item ids seen while a full replay is in flight, or `null` when no
   * replay is being collected. `dropStale` uses the completed set to tell a
   * cached item the server still has from one it no longer has.
   */
  private replayIds: Set<string> | null = null;

  /** Start collecting server item ids (called before `thread/resume`). */
  beginReplay(): void {
    this.replayIds = new Set();
  }

  /**
   * Stop collecting and hand back the ids seen. Returns `null` when no replay
   * was in flight, so a caller cannot accidentally reconcile with an empty set.
   */
  endReplay(): Set<string> | null {
    const ids = this.replayIds;
    this.replayIds = null;
    return ids;
  }

  /**
   * Adopt the server's authoritative content generation for this thread.
   *
   * When the generation changed, everything we hold is structurally stale —
   * the server no longer has these items (it was `/clear`ed) — so the whole
   * transcript is dropped. Returns `true` to tell the caller a full
   * `thread/resume` is required.
   *
   * A same-generation call only advances the cursor: "anchor present but
   * behind" is the normal shape of two clients coexisting, not an error
   * (spec §7.2).
   */
  adoptEpoch(epoch: number, lastItemId: string | null): boolean {
    const changed = this.contentEpoch !== null && this.contentEpoch !== epoch;
    this.contentEpoch = epoch;
    if (changed) {
      this.items = [];
      this.lastServerItemId = null;
      this.openReasoning = null;
      this.epochInvalidated = true;
      return true;
    }
    if (lastItemId !== null) this.lastServerItemId = lastItemId;
    return false;
  }

  /**
   * Drop every item the server did not just replay.
   *
   * Called only after a *full* replay (a successful `thread/resume`), where the
   * server re-sent its entire history: any server item we hold that is absent
   * from `serverIds` no longer exists server-side (cleared, compacted, or
   * trimmed away) and must not linger as ghost content. Local echoes and notices
   * are ours, not the server's — they are always kept.
   */
  dropStale(serverIds: Set<string>): void {
    this.items = this.items.filter(
      (i) => Session.isLocalId(i.id) || serverIds.has(i.id),
    );
    // The cursor must point at a surviving server item (or nothing), never at a
    // dropped one: otherwise a later `readItems(afterItemId)` asks for a window
    // anchored past content the transcript no longer holds.
    for (let i = this.items.length - 1; i >= 0; i -= 1) {
      const id = this.items[i].id;
      if (!Session.isLocalId(id)) {
        this.lastServerItemId = id;
        return;
      }
    }
    this.lastServerItemId = null;
  }

  /**
   * Drop all accumulated state. Must be called *before* issuing
   * `thread/resume`, because replay notifications can arrive before the resume
   * response — resetting after would wipe the freshly replayed history.
   *
   * Replaces `items` with a new array (rather than truncating in place) so
   * React's identity-based memoization notices the change.
   */
  reset(): void {
    this.items = [];
    this.turnActive = false;
    this.lastStatus = null;
    this.lastError = null;
    this.usage = null;
    this.returnedInterjections = [];
    this.lastServerItemId = null;
    this.openReasoning = null;
    this.epochInvalidated = false;
  }

  /**
   * 本地乐观回声：用户刚发出的那条消息。
   *
   * `attachments` 是**发送时**的本地附件（名称/路径/大小），只用于立刻渲染出
   * 带附件的用户气泡。服务端项是权威——它一到，这条本地回声就被就地替换（见
   * `apply` / `upsertItems`），那时展示的才是服务端的元数据。没有附件时字段不
   * 出现（`...(…)` 展开），以免凭空造出一个空数组字段与服务端形状不符。
   */
  addUserMessage(text: string, attachments: Attachment[] = []): void {
    // 用户又发了一条：上一段的 CoT 到此为止，不会与这一轮的思考混成一块。
    this.breakReasoningRun();
    this.items.push({
      type: "userMessage",
      id: nextLocalId(),
      text,
      ...(attachments.length > 0 ? { attachments } : {}),
    });
  }

  /**
   * Whether an item id was minted locally (`local-*`) rather than by the server.
   * Locally-synthesised bubbles (the optimistic echo `send` pushes before the
   * server replies, and `/`-command notices) must never anchor server items:
   * the server transcript is the authority on order.
   */
  private static isLocalId(id: string | undefined): boolean {
    return typeof id === "string" && id.startsWith("local-");
  }

  /**
   * Insert a server item right after the last item that already holds a position.
   *
   * Appending to the array end was wrong whenever a locally-minted bubble sat
   * there: the first server item of a turn (the opening user message) would land
   * *after* the local echo that preceded it, or — for a thread opened while a
   * turn was already running — after agent text that arrived first, which is
   * exactly how the opening prompt ended up at the bottom of the transcript.
   * Anchoring to the last positioned item keeps reload-on-open order.
   *
   * The anchor is a server item **or an already-placed reasoning block**: the CoT
   * block is a local item, but it is positioned by this very logic and must stay
   * ahead of the answer it precedes. If it did not anchor, the turn's
   * `agentMessage` would be spliced in before it and the 「思考」block would end
   * up trailing its own answer (design §6.1: 位于正文之前). Local echoes and
   * notices are still not anchors — the server transcript is the authority.
   */
  private insertAfterLastServerItem(item: Item): void {
    // 任何服务端项一旦插到推理块之后，就该由**它**来承载这一轮的正文：推理块
    // 到此为止（下一段 CoT 另起一块）。不打断的话，`reasoning[上一轮]` 会一直
    // 留在末尾，把下一轮 delta 吸进上一轮的块里。
    this.breakReasoningRun();
    let last = -1;
    for (let i = this.items.length - 1; i >= 0; i -= 1) {
      const held = this.items[i];
      if (!Session.isLocalId(held.id) || held.type === "reasoning") {
        last = i;
        break;
      }
    }
    if (last < 0) this.items.push(item);
    else this.items.splice(last + 1, 0, item);
  }

  /**
   * 关闭当前打开的推理块：下一段 `thread/reasoningDelta` 会新起一块。
   *
   * 站在「合并成一块」的反面——它只让**连续**的 delta 合并，别的任何条目（服务端
   * 正文/工具/用户气泡，或本地 notice）落进来就断开。
   */
  private breakReasoningRun(): void {
    this.openReasoning = null;
  }

  /** Append a command-output line (never sent to the agent). */
  notice(text: string): void {
    this.breakReasoningRun();
    this.items.push({ type: "notice", id: nextLocalId(), text });
  }

  /**
   * Append a command-output notice that carries structured data — `/config`
   * hands over the raw `config/read` result (or its error) for `ChatView` to
   * render as a card. `text` stays empty: the card is the presentation, and no
   * other consumer reads it.
   */
  noticeConfig(data: Record<string, unknown> | null, error?: string | null): void {
    this.breakReasoningRun();
    this.items.push({
      type: "notice",
      id: nextLocalId(),
      text: "",
      config: { data, error: error ?? null },
    });
  }

  /**
   * Remove the local optimistic echo for `text` (a rejected `send`).
   *
   * Matched by id-prefix + text rather than "the last item": once the server's
   * opening item for a turn exists, the local echo is no longer necessarily the
   * array tail, and popping blindly could delete the wrong bubble.
   */
  dropLocalUserMessage(text: string): void {
    const index = this.items.findIndex(
      (i) => Session.isLocalId(i.id) && i.type === "userMessage" && i.text === text,
    );
    if (index >= 0) this.items.splice(index, 1);
  }

  /**
   * Merge a batch of server items fetched via `thread/readItems` (cold-return
   * catch-up). De-duplicates by protocol `id` — an item already rendered
   * (because it arrived live or was fetched before) is replaced in place, an
   * unseen one is appended — so replaying an overlap is harmless. Advances
   * {@link lastServerItemId} to the last incoming id, keeping the cursor
   * monotonic with what the transcript now holds.
   */
  upsertItems(items: Item[]): void {
    // 回放/补齐的服务端项同样打断推理块：否则下一轮 delta 会续进上一轮那块。
    this.breakReasoningRun();
    let reconciledLocalEcho = false;
    for (const item of items) {
      // Same normalisation as `apply`: a mid-turn interjection renders as a user
      // bubble, so a cold replay must not introduce a second shape.
      const normalized: Item =
        item.type === "userInterjection"
          ? { type: "userMessage", id: item.id, text: item.text }
          : item;
      const index = this.items.findIndex((i) => i.id === normalized.id);
      if (index >= 0) {
        this.items[index] = normalized;
      } else if (normalized.type === "userMessage") {
        // 回放里还没有这条用户气泡:先把本地乐观回声就地换成服务端项,再去重。
        // 否则「本地用户气泡 + 服务端同一条」会被渲染两次。
        const local = this.items.findIndex(
          (i) => Session.isLocalId(i.id) && i.type === "userMessage" && i.text === normalized.text,
        );
        if (local >= 0) {
          this.items[local] = normalized;
          reconciledLocalEcho = true;
        } else {
          this.insertAfterLastServerItem(normalized);
        }
      } else {
        this.insertAfterLastServerItem(normalized);
      }
      this.lastServerItemId = normalized.id;
    }
    // 之后仍有多余的本地回声(同一 prompt 的其它乐观气泡)是无主副本,丢弃。
    if (reconciledLocalEcho) {
      this.dropDanglingLocalEchoes();
    }
  }

  /**
   * Drop local user bubbles that duplicate a server user message in the replay.
   *
   * `send` pushes a local echo before the server replies; once the server's own
   * opening item for that turn arrives (live or on replay), the local copy is a
   * stray duplicate. Matching on text is safe here because a *reconciled* local
   * copy was already replaced in place by the server item, so what remains are
   * only the extras.
   */
  private dropDanglingLocalEchoes(): void {
    const serverTexts = new Set(
      this.items
        .filter((i) => i.type === "userMessage" && !Session.isLocalId(i.id))
        .map((i) => (i as { text: string }).text),
    );
    for (let i = this.items.length - 1; i >= 0; i -= 1) {
      const item = this.items[i];
      if (Session.isLocalId(item.id) && item.type === "userMessage" && serverTexts.has(item.text)) {
        this.items.splice(i, 1);
      }
    }
  }

  apply(notification: Notification): void {
    switch (notification.method) {
      case "item/started":
      case "item/completed": {
        const incoming = notification.params.item;
        // A mid-turn interjection is normalised to a user bubble: the transcript
        // should read as a conversation, and the distinct protocol type exists
        // so the distinction is visible on the wire, not to require a second
        // renderer here.
        const item: Item =
          incoming.type === "userInterjection"
            ? { type: "userMessage", id: incoming.id, text: incoming.text }
            : incoming;
        if (this.replayIds !== null) this.replayIds.add(item.id);
        // 服务端 thinking item 是权威：实时期间前端先用本地 `reasoning` 块保留流式
        // 手感，本 item 一到就**就地接管**——在原块的位置换成服务端项（位置不变），
        // 并记下服务端 id，使随后的 `item/completed` 按 id 就地替换而不重复。接管后
        // 运行断开，其后到达的 delta 另起一块本地块（design §6.1 单一渲染路径）。
        //
        // 仅在**本服务端 id 尚未入转录**时才接管：否则配对的 `item/completed`
        // （此刻 openReasoning 可能已是 started/completed 之间新起的本地块）会
        // 误吞那个新块并产生重复的服务端项。已存在则走下面的按 id 就地替换。
        if (
          item.type === "thinking" &&
          this.openReasoning &&
          this.items.findIndex((i) => i.id === item.id) < 0
        ) {
          const held = this.items.findIndex((i) => i.id === this.openReasoning!.id);
          if (held >= 0) {
            this.items[held] = item;
            this.lastServerItemId = item.id;
            this.breakReasoningRun();
            break;
          }
        }
        // 任何服务端项都断开推理块的合并运行：正文/工具/用户气泡一到，当前块结束。
        this.breakReasoningRun();
        const index = this.items.findIndex((i) => i.id === item.id);
        if (index >= 0) {
          this.items[index] = item;
        } else if (
          item.type === "userMessage" &&
          this.items.some((i) => Session.isLocalId(i.id))
        ) {
          // 服务端发来的本轮开启项:先认领对应的本地乐观回声(就地替换),再去重。
          const local = this.items.findIndex(
            (i) => Session.isLocalId(i.id) && i.type === "userMessage" && i.text === item.text,
          );
          if (local >= 0) this.items[local] = item;
          else this.insertAfterLastServerItem(item);
        } else if (item.type === "agentMessage") {
          // agent 文本是**续写**:锚定在最后一个服务端项之后,不落在本地回声之后
          // (否则同一轮的 agent 回复会排到用户气泡前面)。
          this.insertAfterLastServerItem(item);
        } else if (item.type === "thinking") {
          // 回放/未接管的服务端思考条目:与其他服务端项同样锚定在最后一个服务端项
          // 之后——它必须停在自己那一轮的答案之前(design §6.1 位于正文之前)。
          this.insertAfterLastServerItem(item);
        } else {
          this.items.push(item);
        }
        this.lastServerItemId = item.id;
        break;
      }
      case "items/completed": {
        // 回放期的批量帧：交给 upsertItems（按 id 去重/就地替换/保持顺序/
        // 推进 lastServerItemId），与逐条 item/completed 等价且幂等。
        if (this.replayIds !== null) {
          for (const it of notification.params.items) this.replayIds.add(it.id);
        }
        this.upsertItems(notification.params.items);
        break;
      }
      case "item/delta": {
        // Text resumed: the retry succeeded, so the notice has served its purpose.
        this.retrying = null;
        const { item_id, delta } = notification.params;
        if (this.replayIds !== null) this.replayIds.add(item_id);
        this.lastServerItemId = item_id;
        // 正文增量同样打断推理块：一旦 agent 文本开始流，当前 CoT 块就结束了，
        // 块不会因此移位（正文会插到它之后）。
        this.breakReasoningRun();
        const index = this.items.findIndex((i) => i.id === item_id);
        if (index < 0) {
          this.insertAfterLastServerItem({ type: "agentMessage", id: item_id, text: delta });
        } else if (this.items[index].type === "agentMessage") {
          const item = this.items[index] as { type: "agentMessage"; id: string; text: string };
          item.text += delta;
        }
        // else: delta for a non-agent item (e.g. streamed tool output) — ignore
        // in the baseline UI.
        break;
      }
      case "thread/reasoningDelta": {
        // CoT 累积进同一个推理块：只在**当前这一轮**的连续 delta 之间合并，其他
        // 任何条目（助手正文、工具卡、notice、下一轮开启项）都已断开运行
        // （见 `breakReasoningRun`）→ 新起一块。与 TUI 的 Reasoning cell 同契约。
        //
        // 块用一个稳定锚点（openReasoning），而不是「数组末尾」：服务端项会被
        // 插到最后一个服务端项之后，若以末尾为准，本轮的 agentMessage 会把块甩到
        // 它自己的答案后面（design §6.1 要求块位于正文之前）。
        const { text } = notification.params;
        if (this.openReasoning) {
          this.openReasoning.text += text;
        } else {
          const block: ReasoningItem = { type: "reasoning", id: nextLocalId(), text };
          this.openReasoning = block;
          this.items.push(block);
        }
        break;
      }
      case "thread/notice":
        // 内容层告知（通道回退 / effort 降级）：复用本地 notice 形状，带上级别
        // 以便 Warn 变体可样式化。它**不是**服务端 item，不该进 lastServerItemId。
        // 它会插入转录、打断推理块的合并运行。
        this.breakReasoningRun();
        this.items.push({
          type: "notice",
          id: nextLocalId(),
          text: notification.params.text,
          level: notification.params.level,
        });
        break;
      case "turn/started":
        // 新的一轮：上一轮的 CoT 到此为止，本轮 CoT 另起一块。这是「按轮分块」的
        // 兜底保证——即便上一轮以 turn/completed 收尾（而非某项）而运行尚未合上，
        // 新轮开始也一定断开。
        this.breakReasoningRun();
        this.turnActive = true;
        this.lastError = null;
        break;
      case "turn/completed":
        this.turnActive = false;
        this.retrying = null;
        this.lastStatus = notification.params.status;
        this.lastError = notification.params.error ?? null;
        break;
      case "turn/interjectionsReturned":
        // Ordered ahead of `turn/completed` by the server, so the text is
        // restorable before the turn is marked finished.
        this.returnedInterjections = [...notification.params.items];
        break;
      case "turn/retry":
        this.retrying = {
          attempt: notification.params.attempt,
          max: notification.params.max,
          // 线形状是 `string`；这里收窄到 `RetryCause`（见 `toRetryCause` 的注释）。
          cause: toRetryCause(notification.params.cause),
        };
        break;
      case "thread/tokenUsage/updated":
        this.usage = {
          model: notification.params.model,
          input: notification.params.input_tokens,
          output: notification.params.output_tokens,
          cacheWrite: notification.params.cache_creation_input_tokens ?? 0,
          cacheRead: notification.params.cache_read_input_tokens ?? 0,
        };
        break;
      case "error":
        this.lastError = notification.params.message;
        break;
      default:
        break;
    }
  }
}
