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
  items: (Item | NoticeItem)[] = [];
  turnActive = false;
  lastStatus: TurnStatus | null = null;
  lastError: string | null = null;
  usage: Usage | null = null;
  retrying: { attempt: number; max: number; cause: RetryCause } | null = null;

  /**
   * 服务端 item 流里最新的、带服务端 id 的 item id（本地生成的用户气泡/通知
   * 不计入）。它同时是"续读游标"：远程客户端从冷会话返回时，用 `thread/readItems`
   * 带 `afterItemId` 只补齐这之后的内容。`null` 表示还没有任何服务端 item。
   */
  lastServerItemId: string | null = null;

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
        this.upsertItems(notification.params.items);
        break;
      }
      case "item/delta": {
        // Text resumed: the retry succeeded, so the notice has served its purpose.
        this.retrying = null;
        const { item_id, delta } = notification.params;
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
