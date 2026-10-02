import type { Item, Notification, RetryCause, TurnStatus, Usage } from "./protocol";

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

  addUserMessage(text: string): void {
    this.items.push({ type: "userMessage", id: nextLocalId(), text });
  }

  /** Append a command-output line (never sent to the agent). */
  notice(text: string): void {
    this.items.push({ type: "notice", id: nextLocalId(), text });
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
    for (const item of items) {
      // Same normalisation as `apply`: a mid-turn interjection renders as a user
      // bubble, so a cold replay must not introduce a second shape.
      const normalized: Item =
        item.type === "user_interjection"
          ? { type: "userMessage", id: item.id, text: item.text }
          : item;
      const index = this.items.findIndex((i) => i.id === normalized.id);
      if (index >= 0) this.items[index] = normalized;
      else this.items.push(normalized);
      this.lastServerItemId = normalized.id;
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
        if (index >= 0) this.items[index] = item;
        else this.items.push(item);
        this.lastServerItemId = item.id;
        break;
      }
      case "item/delta": {
        // Text resumed: the retry succeeded, so the notice has served its purpose.
        this.retrying = null;
        const { item_id, delta } = notification.params;
        this.lastServerItemId = item_id;
        const index = this.items.findIndex((i) => i.id === item_id);
        if (index < 0) {
          this.items.push({ type: "agentMessage", id: item_id, text: delta });
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
