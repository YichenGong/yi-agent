import type { Item } from "./protocol";
import type { NoticeItem, ReasoningItem } from "./session";

/**
 * The text an item renders, lifted out as a primitive.
 *
 * For a prefix item this is the written-once value and it never moves again.
 * For the tail it is re-read on every render from the very object `Session`
 * mutates in place — and it is what `ChatItem`'s comparator compares, and what
 * the agent-message branch renders. The comparator must never read
 * `item.text`; see `ChatItem` in `components/ChatView.tsx` for why.
 *
 * 也供搜索索引使用：搜索必须在数据层产出与渲染完全一致的文本，
 * 否则「搜到的文案」与「画出来的文案」会分叉。
 */
export function itemText(item: Item | NoticeItem | ReasoningItem): string {
  switch (item.type) {
    case "agentMessage":
    case "userMessage":
    case "reasoning":
    // `display:"omitted"` 的 thinking 明文是空串，呈现由 `redacted` 决定；text 仍
    // 要照传，好让就地追加能被比较器看见。
    case "thinking":
      return item.text;
    // 普通文本 notice（缺省 `body`）在 `ChatView` 里就渲染 `item.text`，所以它必须
    // 可搜；带结构化载荷的 notice 渲染成卡片，正文不是 `text`，按空串处理。
    case "notice":
      return item.body ? "" : item.text;
    default:
      // `toolCall` 无 text。
      // `userInterjection` 也落到这里，但它在进入 `items` 前已被归一化成
      // `userMessage`（见 `Session.upsertItems` / `Session.apply`），无需单独处理。
      return "";
  }
}
