import { memo, useEffect, useRef } from "react";
import type { Item, RetryCause } from "../lib/protocol";
import type { NoticeItem } from "../lib/session";
import { AgentMessage } from "./MarkdownText";
import { ToolCallCard } from "./ToolCallCard";

/**
 * `ToolCallCard` memoized on its `item` prop.
 *
 * `Session.apply` replaces the slot with a *new* item object when a tool call
 * starts or completes, so identity is a sound signal here: the same object means
 * the card cannot have changed. Without this boundary every streamed text delta
 * re-rendered every settled tool card (re-serializing its JSON with
 * `JSON.stringify(item.input, null, 2)`), which is unbounded work that grows with
 * the transcript and competes with the sidebar spinner for the main thread.
 */
const ToolCallRow = memo(ToolCallCard);

/**
 * One transcript item.
 *
 * The memo boundary sits on the *leaf* components and their value props, not on
 * the item object: `item/delta` mutates the trailing agent message **in place**,
 * so a memo keyed on object identity would never see the text change and the
 * streamed reply would freeze. Passing `text` (a primitive) lets React's default
 * shallow prop comparison notice the change while leaving every other message —
 * whose text did not move — untouched.
 */
function ChatItem({ item }: { item: Item | NoticeItem }) {
  switch (item.type) {
    case "notice":
      return (
        <div className="my-1 self-center rounded-md bg-neutral-800/60 px-3 py-1 text-xs text-neutral-400">
          {item.text}
        </div>
      );
    case "userMessage":
      return (
        <div className="my-1 max-w-[80%] self-end rounded-lg bg-blue-600 px-3 py-2 text-sm whitespace-pre-wrap text-white">
          {item.text}
        </div>
      );
    case "agentMessage":
      return <AgentMessage text={item.text} />;
    case "toolCall":
      return <ToolCallRow item={item} />;
    default: {
      // Defensive: the server sent an item type this build does not know
      // about. Surface it rather than silently dropping the item.
      const unknown = item as { type?: string };
      return (
        <div className="my-1 font-mono text-xs text-neutral-500">
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
}: {
  items: (Item | NoticeItem)[];
  error?: string | null;
  retrying?: { attempt: number; max: number; cause: RetryCause } | null;
}) {
  const endRef = useRef<HTMLDivElement>(null);

  // `items` is `session.items` — the same array instance for the whole session,
  // mutated in place (push / items[i] = x). React compares deps with Object.is,
  // so depending on `items` would never re-run this effect when a streamed delta
  // appends text. Depend on values that actually change instead: the item count
  // and the trailing agent text.
  const lastItem = items[items.length - 1];
  const lastText = lastItem && lastItem.type === "agentMessage" ? lastItem.text : "";

  useEffect(() => {
    endRef.current?.scrollIntoView({ block: "end" });
  }, [items.length, lastText, error]);

  return (
    <div className="flex flex-1 flex-col overflow-y-auto px-4 py-3">
      {items.map((item, index) => (
        // `index` is only a fallback key for defensive unknown-type items; every
        // known item carries a stable protocol `id`.
        <ChatItem key={item.id ?? index} item={item} />
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
      <div ref={endRef} />
    </div>
  );
}
