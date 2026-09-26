import { useEffect, useRef } from "react";
import type { Item, RetryCause } from "../lib/protocol";
import { MarkdownText } from "./MarkdownText";
import { ToolCallCard } from "./ToolCallCard";

export function ChatView({
  items,
  error,
  retrying,
}: {
  items: Item[];
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
      {items.map((item, index) => {
        switch (item.type) {
          case "userMessage":
            return (
              <div
                key={item.id}
                className="my-1 max-w-[80%] self-end rounded-lg bg-blue-600 px-3 py-2 text-sm whitespace-pre-wrap text-white"
              >
                {item.text}
              </div>
            );
          case "agentMessage":
            return (
              <div key={item.id} className="my-1 max-w-[90%] self-start">
                <MarkdownText text={item.text} />
              </div>
            );
          case "toolCall":
            return <ToolCallCard key={item.id} item={item} />;
          default: {
            // Defensive: the server sent an item type this build does not know
            // about. Surface it rather than silently dropping the item.
            const unknown = item as { type?: string };
            return (
              <div key={index} className="my-1 font-mono text-xs text-neutral-500">
                [unsupported item type: {unknown.type ?? "unknown"}]
              </div>
            );
          }
        }
      })}
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
