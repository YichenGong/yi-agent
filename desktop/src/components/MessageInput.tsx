import { useState } from "react";
import { ModeChip } from "./ModeChip";
import type { ThreadMode } from "../lib/threadPermissionMode";

export function MessageInput({
  turnActive,
  onSend,
  onInterrupt,
  mode,
  onModeChange,
}: {
  turnActive: boolean;
  onSend: (text: string) => Promise<boolean>;
  onInterrupt: () => void;
  mode: ThreadMode | null;
  onModeChange: (mode: ThreadMode) => void;
}) {
  const [text, setText] = useState("");
  const [sending, setSending] = useState(false);

  const handleSend = async () => {
    if (!text.trim() || sending) return;
    setSending(true);
    try {
      const ok = await onSend(text);
      // Only discard the draft once the send was actually accepted; otherwise
      // the user's text would be lost on a rejected turn/start.
      if (ok) setText("");
    } finally {
      setSending(false);
    }
  };

  return (
    <div className="flex items-end gap-2 border-t border-neutral-800 bg-neutral-900 p-3">
      <textarea
        value={text}
        onChange={(e) => setText(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter" && !e.shiftKey) {
            e.preventDefault();
            if (turnActive) onInterrupt();
            else void handleSend();
          }
        }}
        disabled={sending}
        rows={3}
        placeholder="Type a message… (Enter to send, Shift+Enter for newline)"
        className="flex-1 resize-none rounded-md border border-neutral-700 bg-neutral-950 px-3 py-2 text-sm text-neutral-100 placeholder:text-neutral-600 focus:border-neutral-500 focus:outline-none disabled:opacity-50"
      />
      <ModeChip mode={mode} onChange={onModeChange} disabled={mode === null} />
      <button
        type="button"
        onClick={turnActive ? onInterrupt : () => void handleSend()}
        disabled={sending}
        className={
          turnActive
            ? "rounded-md bg-red-600 px-4 py-2 text-sm font-medium text-white hover:bg-red-500 disabled:opacity-50"
            : "rounded-md bg-blue-600 px-4 py-2 text-sm font-medium text-white hover:bg-blue-500 disabled:opacity-50"
        }
      >
        {turnActive ? "Stop" : "Send"}
      </button>
    </div>
  );
}
