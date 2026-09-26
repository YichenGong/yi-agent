import { useEffect, useRef } from "react";
import type { Item } from "../lib/protocol";
import { ToolCallCard } from "./ToolCallCard";

export function ChatView({ items, error }: { items: Item[]; error?: string | null }) {
  const endRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    endRef.current?.scrollIntoView();
  }, [items, error]);

  return (
    <div className="flex flex-1 flex-col overflow-y-auto px-4 py-3">
      {items.map((item) => {
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
              <div
                key={item.id}
                className="my-1 max-w-[90%] self-start font-mono text-sm whitespace-pre-wrap text-neutral-100"
              >
                {item.text}
              </div>
            );
          case "toolCall":
            return <ToolCallCard key={item.id} item={item} />;
        }
      })}
      {error && (
        <div className="my-2 rounded-md border border-red-500/40 bg-red-500/10 px-3 py-2 text-sm text-red-300">
          {error}
        </div>
      )}
      <div ref={endRef} />
    </div>
  );
}
