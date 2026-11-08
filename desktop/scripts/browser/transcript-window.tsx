// Browser-only fixture for the **long-session** geometry of `ChatView`.
//
// The other fixtures in this directory use 40 items, so every item is mounted and
// both spacers are always 0px. That means they structurally cannot catch the two
// defects this file exists for:
//
// 1. spacers being flex-shrunk to 0 height, and
// 2. the transcript window being inherited across a session switch.
//
// A/B are two equal-length sessions (600 items each) with distinct id prefixes so
// the checks can tell which slice of which session ended up mounted.
import { useRef, useState } from "react";
import { flushSync } from "react-dom";
import { createRoot } from "react-dom/client";
import { ChatView } from "../../src/components/ChatView";
import type { ViewAnchor } from "../../src/lib/viewAnchor";
import type { Item } from "../../src/lib/protocol";
import "../../src/index.css";

const COUNT = 600;

function makeSession(prefix: string): Item[] {
  return Array.from({ length: COUNT }, (_, i) => ({
    type: "userMessage" as const,
    id: `${prefix}-${i}`,
    // Tall enough that 60 items overflow the viewport, which is what makes the
    // unmounted range (and therefore the top spacer) non-zero.
    text: `${prefix} message ${i}\n` + "line\n".repeat(8),
  }));
}

const sessions: Record<string, Item[]> = { A: makeSession("A"), B: makeSession("B") };
const anchors = new Map<string, ViewAnchor | null>();

function Fixture() {
  const [id, setId] = useState<"A" | "B">("A");
  const capture = useRef<(() => void) | null>(null);

  // Same contract as `App.selectThread`: flush the outgoing view's position before
  // selecting the next session.
  const select = (next: "A" | "B") => {
    capture.current?.();
    flushSync(() => setId(next));
  };

  Object.assign(window, {
    probe: {
      select,
      getId: () => id,
      items: (which: "A" | "B") => sessions[which].length,
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
      />
    </div>
  );
}

createRoot(document.getElementById("root")!).render(<Fixture />);
