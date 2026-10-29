// Browser-only fixture: real ChatView, React commits, CSS, DOM geometry and scroll events.
import { useRef, useState } from "react";
import { flushSync } from "react-dom";
import { createRoot } from "react-dom/client";
import { ChatView } from "../../src/components/ChatView";
import type { ViewAnchor } from "../../src/lib/viewAnchor";
import "../../src/index.css";

const lists = Object.fromEntries(["A", "B"].map(id => [id,
  Array.from({ length: 40 }, (_, i) => ({
    type: "userMessage" as const, id: `${id}-${i}`,
    text: `${id} message ${i}\n` + "line\n".repeat(8),
  })),
]));
const anchors = new Map<string, ViewAnchor | null>();

function Fixture() {
  const [id, setId] = useState("A");
  const [count, setCount] = useState(40);
  const [mount, setMount] = useState(0);
  const [errors, setErrors] = useState<Record<string, string>>({});
  const capture = useRef<(() => void) | null>(null);
  // Like App.selectThread: capture the outgoing DOM BEFORE selecting the next thread.
  const select = (next: string) => {
    capture.current?.();
    flushSync(() => { setId(next); setCount(40); });
  };
  Object.assign(window, { probe: {
    select, selectSync: select,
    cold: (next: string) => { capture.current?.(); setId(next); setCount(0); },
    error: (owner: string, text: string) => setErrors(old => ({...old, [owner]: text})),
    replay: setCount, remount: () => setMount(v => v + 1), anchors, getId: () => id,
  } });
  return <div style={{ height: 500, display: "flex", flexDirection: "column", width: 650 }}>
    <ChatView key={mount} captureViewRef={capture} items={lists[id].slice(0, count)}
      error={errors[id]} syncing={count < 40} threadId={id} anchor={anchors.get(id)}
      onRememberView={(owner, anchor) => anchors.set(owner, anchor)} />
  </div>;
}
createRoot(document.getElementById("root")!).render(<Fixture />);
