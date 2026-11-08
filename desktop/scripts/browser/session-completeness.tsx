// Real-CSS/real-layout fixture for the D-group (display convergence) checks.
//
// jsdom cannot host these: every D assertion is about geometry (what is mounted,
// where the reader's anchor physically sits, whether the scrollbar reserves the
// unmounted range). No sidecar, no LLM, no production session data.
//
// Determinism rules for this fixture:
// - every item is a fixed 8-line block, so "same geometry" comparisons are exact;
// - image/late-growth cases are driven explicitly by `grow`, never by the network;
// - `setCount`/`replayChunk` mutate the array in place like `Session.apply` does.
import { useRef, useState } from "react";
import { flushSync } from "react-dom";
import { createRoot } from "react-dom/client";
import { ChatView } from "../../src/components/ChatView";
import type { TranscriptWindowApi } from "../../src/hooks/useTranscriptWindow";
import type { ViewAnchor } from "../../src/lib/viewAnchor";
import type { Item } from "../../src/lib/protocol";
import "../../src/index.css";

type SessionId = "A" | "B";

function makeItems(prefix: string, count: number): Item[] {
  return Array.from({ length: count }, (_, i) => ({
    type: "userMessage" as const,
    id: `${prefix}-${i}`,
    text: `${prefix} message ${i}\n` + "line\n".repeat(8),
  }));
}

const sessions: Record<SessionId, Item[]> = { A: makeItems("A", 600), B: makeItems("B", 600) };
const anchors = new Map<string, ViewAnchor | null>();
// Growing a row means replacing the item object (that is what an image load does
// to a markdown row); the id stays stable so anchors survive.
const grown = new Map<string, number>();

function Fixture() {
  const [id, setId] = useState<SessionId>("A");
  const [, bump] = useState(0);
  const capture = useRef<(() => void) | null>(null);
  const windowApi = useRef<TranscriptWindowApi | null>(null);

  const select = (next: SessionId) => {
    capture.current?.();
    flushSync(() => setId(next));
  };

  /** Replace `count` items starting at `start`, mimicking a chunked replay. */
  const replayChunk = (start: number, count: number, target: SessionId = id) => {
    const items = sessions[target];
    if (start >= items.length) return;
    flushSync(() => {
      for (let i = start; i < Math.min(items.length, start + count); i += 1) {
        items[i] = { ...(items[i] as { type: "userMessage"; id: string }), text: `${items[i].id}\n` + "line\n".repeat(8) } as Item;
      }
      bump(n => n + 1);
    });
  };

  const setCount = (count: number) => {
    const items = sessions[id];
    flushSync(() => {
      if (count < items.length) items.length = count;
      else for (let i = items.length; i < count; i += 1) items.push({ type: "userMessage", id: `${id}-${i}`, text: `${id} message ${i}\n` + "line\n".repeat(8) });
      bump(n => n + 1);
    });
  };

  /** Make one row physically taller without changing its id. */
  const grow = (itemId: string, lines: number) => {
    const items = sessions[id];
    const at = items.findIndex(item => item.id === itemId);
    if (at < 0) return;
    grown.set(itemId, lines);
    flushSync(() => {
      items[at] = {
        ...(items[at] as { id: string }),
        type: "userMessage",
        text: `${itemId} grown\n` + "line\n".repeat(lines),
      } as Item;
      bump(n => n + 1);
    });
  };

  Object.assign(window, {
    displayProbe: {
      select,
      setCount,
      replayChunk,
      grow,
      retry: () => windowApi.current?.retry(),
      scrollTo: (top: number) => {
        const scroller = document.querySelector<HTMLElement>("[data-thread-id]");
        if (scroller) scroller.scrollTop = top;
      },
      snapshot: () => {
        const scroller = document.querySelector<HTMLElement>("[data-thread-id]");
        const rows = [...document.querySelectorAll<HTMLElement>("[data-item-id]")];
        if (!scroller) return { threadId: null, count: 0, mounted: 0, visibleIds: [], top: 0, following: false };
        const box = scroller.getBoundingClientRect();
        return {
          threadId: scroller.dataset.threadId ?? null,
          count: sessions[id].length,
          mounted: rows.length,
          visibleIds: rows
            .filter(row => {
              const r = row.getBoundingClientRect();
              return r.bottom > box.top && r.top < box.bottom && r.height > 0;
            })
            .map(row => row.dataset.itemId ?? ""),
          top: scroller.scrollTop,
          maxTop: scroller.scrollHeight - scroller.clientHeight,
          following: scroller.scrollHeight - scroller.clientHeight - scroller.scrollTop <= 4,
          displayError: windowApi.current?.displayError ?? null,
          windowStart: windowApi.current?.window.start ?? null,
          windowEnd: windowApi.current?.window.end ?? null,
        };
      },
    },
  });

  return (
    <div style={{ height: 820, display: "flex", flexDirection: "column", width: 900 }}>
      <ChatView
        items={sessions[id]}
        threadId={id}
        anchor={anchors.get(id)}
        onRememberView={(owner, anchor) => anchors.set(owner, anchor)}
        captureViewRef={capture}
        windowApiRef={windowApi}
      />
    </div>
  );
}

createRoot(document.getElementById("root")!).render(<Fixture />);
