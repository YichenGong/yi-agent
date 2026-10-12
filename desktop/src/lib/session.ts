import type { Attachment, Item, Notification, RetryCause, TurnStatus, Usage } from "./protocol";

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
 * 一段累积的思维链（CoT）。**本地展示条目，不是 wire `Item`**：服务端从不把它作为
 * item 发送，只以 `thread/reasoningDelta` 流式推送；它同样绝不回灌进后续轮次。
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
   * 本会话的 reasoning effort 覆盖：`thread/setReasoningEffort`/`thread/start`/
   * `thread/resume` 的生效值，或 `thread/reasoningEffortChanged` 的推送。`null` =
   * **无会话覆盖**（回落清单条目默认），不是「未设置」——未设置也表现为本会话
   * 可能正跑着条目默认档。控件据此回显。
   */
  reasoningEffort: string | null = null;

  /**
   * 服务端 item 流里最新的、带服务端 id 的 item id（本地生成的用户气泡/通知
   * 不计入）。它同时是"续读游标"：远程客户端从冷会话返回时，用 `thread/readItems`
   * 带 `afterItemId` 只补齐这之后的内容。`null` 表示还没有任何服务端 item。
   */
  lastServerItemId: string | null = null;

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
    this.reasoningEffort = null;
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
   * Insert a server item right after the last server item already held.
   *
   * Appending to the array end was wrong whenever a locally-minted bubble sat
   * there: the first server item of a turn (the opening user message) would land
   * *after* the local echo that preceded it, or — for a thread opened while a
   * turn was already running — after agent text that arrived first, which is
   * exactly how the opening prompt ended up at the bottom of the transcript.
   * Anchoring to the last server item keeps reload-on-open order.
   */
  private insertAfterLastServerItem(item: Item): void {
    let last = -1;
    for (let i = this.items.length - 1; i >= 0; i -= 1) {
      if (!Session.isLocalId(this.items[i].id)) {
        last = i;
        break;
      }
    }
    if (last < 0) this.items.push(item);
    else this.items.splice(last + 1, 0, item);
  }

  /** Append a command-output line (never sent to the agent). */
  notice(text: string): void {
    this.items.push({ type: "notice", id: nextLocalId(), text });
  }

  /**
   * Append a command-output notice that carries structured data — `/config`
   * hands over the raw `config/read` result (or its error) for `ChatView` to
   * render as a card. `text` stays empty: the card is the presentation, and no
   * other consumer reads it.
   */
  noticeConfig(data: Record<string, unknown> | null, error?: string | null): void {
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
    let reconciledLocalEcho = false;
    for (const item of items) {
      // Same normalisation as `apply`: a mid-turn interjection renders as a user
      // bubble, so a cold replay must not introduce a second shape.
      const normalized: Item =
        item.type === "user_interjection"
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
          incoming.type === "user_interjection"
            ? { type: "userMessage", id: incoming.id, text: incoming.text }
            : incoming;
        if (this.replayIds !== null) this.replayIds.add(item.id);
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
        // CoT 累积进同一个推理块：只在末尾**仍是**推理块时追加，其他任何条目
        // （助手正文、工具卡、notice）都另起一块——与 TUI 的 Reasoning cell 同契约。
        const { text } = notification.params;
        const last = this.items[this.items.length - 1];
        if (last && last.type === "reasoning") {
          last.text += text;
        } else {
          this.items.push({ type: "reasoning", id: nextLocalId(), text });
        }
        break;
      }
      case "thread/notice":
        // 内容层告知（通道回退 / effort 降级）：复用本地 notice 形状，带上级别
        // 以便 Warn 变体可样式化。它**不是**服务端 item，不该进 lastServerItemId。
        this.items.push({
          type: "notice",
          id: nextLocalId(),
          text: notification.params.text,
          level: notification.params.level,
        });
        break;
      case "thread/reasoningEffortChanged":
        // 会话元数据：更新本会话的覆盖（缺省 = 已清除）。`null` 表示「无覆盖」，
        // 由控件回落条目默认。
        this.reasoningEffort = notification.params.effort ?? null;
        break;
      case "turn/started":
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
          cause: notification.params.cause,
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
