import type { Item, Notification, RetryCause, TurnStatus, Usage } from "./protocol";

let localSeq = 0;
const nextLocalId = () => `local-${++localSeq}`;

/**
 * Accumulates the client-side view of one app-server thread.
 *
 * `apply` folds each incoming notification into `items` / turn / usage state.
 * Items are keyed by their protocol `id`, so `item/completed` replaces the
 * placeholder created by `item/started`, and `item/delta` appends streamed
 * agent text to the existing item in place.
 */
export class Session {
  items: Item[] = [];
  turnActive = false;
  lastStatus: TurnStatus | null = null;
  lastError: string | null = null;
  usage: Usage | null = null;
  retrying: { attempt: number; max: number; cause: RetryCause } | null = null;

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
  }

  addUserMessage(text: string): void {
    this.items.push({ type: "userMessage", id: nextLocalId(), text });
  }

  apply(notification: Notification): void {
    switch (notification.method) {
      case "item/started":
      case "item/completed": {
        const incoming = notification.params.item;
        const index = this.items.findIndex((i) => i.id === incoming.id);
        if (index >= 0) this.items[index] = incoming;
        else this.items.push(incoming);
        break;
      }
      case "item/delta": {
        // Text resumed: the retry succeeded, so the notice has served its purpose.
        this.retrying = null;
        const { item_id, delta } = notification.params;
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
