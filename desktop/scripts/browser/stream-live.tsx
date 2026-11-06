// Browser-only fixture: the real ChatView driven the way `App` drives it during a
// live turn — `Session.apply` mutates the trailing agent message in place, the
// host bumps a `force` tick, and the component re-renders with the SAME `items`
// array. Real layout, real scroll events, no jsdom stand-ins.
import { useRef, useState } from "react";
import { flushSync } from "react-dom";
import { createRoot } from "react-dom/client";
import { ChatView } from "../../src/components/ChatView";
import { Session, type NoticeItem, type ReasoningItem } from "../../src/lib/session";
import type { Item } from "../../src/lib/protocol";
import "../../src/index.css";

const session = new Session();

/** One streamed reply, in the same order the server emits it. */
function startTurn(question: string) {
  session.apply({
    method: "item/started",
    params: { thread_id: "t", item: { type: "userMessage", id: `u-${question}`, text: question } },
  } as never);
  session.apply({
    method: "item/started",
    params: { thread_id: "t", item: { type: "agentMessage", id: `a-${question}`, text: "" } },
  } as never);
}

function delta(id: string, text: string) {
  session.apply({
    method: "item/delta",
    params: { thread_id: "t", item_id: id, delta: text },
  } as never);
}

function Fixture() {
  const [, force] = useState(0);
  const seen = useRef(0);
  Object.assign(window, {
    probe: {
      startTurn: (q: string) => flushSync(() => { startTurn(q); force((v) => v + 1); }),
      // Exactly like `ThreadStore.applyNotification` → App's callback: mutate first,
      // then re-render. React must see the new text through `items` (mutated array).
      delta: (id: string, text: string) =>
        flushSync(() => { delta(id, text); force((v) => v + 1); }),
      reasoning: (text: string) =>
        flushSync(() => {
          session.apply({
            method: "thread/reasoningDelta",
            params: { thread_id: "t", text, kind: "Content" },
          } as never);
          force((v) => v + 1);
        }),
      toolStart: (id: string, name: string) =>
        flushSync(() => {
          session.apply({
            method: "item/started",
            params: {
              thread_id: "t",
              item: { type: "toolCall", id, call_id: id, name, input: { cmd: "ls" }, status: "running" },
            },
          } as never);
          force((v) => v + 1);
        }),
      domText: () => document.querySelector("[data-thread-id]")?.textContent ?? "",
      renders: () => seen.current,
      count: () => session.items.length,
      // Text of one specific item, independent of where it sits in the transcript.
      textOf: (id: string) => {
        const found = session.items.find((i) => i.id === id) as { text?: string } | undefined;
        return found?.text ?? "";
      },
      // Whether a given id is still the last item (i.e. the streaming tail).
      isTail: (id: string) => session.items[session.items.length - 1]?.id === id,
      scrollTop: () => document.querySelector<HTMLElement>("[data-thread-id]")!.scrollTop,
      maxScroll: () => {
        const el = document.querySelector<HTMLElement>("[data-thread-id]")!;
        return el.scrollHeight - el.clientHeight;
      },
      // What the user can actually see right now, in viewport pixels.
      visible: (needle: string) => {
        const el = document.querySelector<HTMLElement>("[data-thread-id]")!;
        const walker = document.createTreeWalker(el, NodeFilter.SHOW_TEXT);
        const box = el.getBoundingClientRect();
        for (let n = walker.nextNode(); n; n = walker.nextNode()) {
          if (!n.textContent || !n.textContent.includes(needle)) continue;
          const range = document.createRange();
          range.selectNodeContents(n);
          const r = range.getBoundingClientRect();
          if (r.bottom > box.top && r.top < box.bottom && r.height > 0) return true;
        }
        return false;
      },
    },
  });
  seen.current += 1;
  return (
    <div style={{ height: 500, display: "flex", flexDirection: "column", width: 650 }}>
      <ChatView
        items={session.items as (Item | NoticeItem | ReasoningItem)[]}
        threadId="t"
        structuralVersion={session.structuralVersion}
        textVersion={session.textVersion}
      />
    </div>
  );
}

createRoot(document.getElementById("root")!).render(<Fixture />);
