import { useState } from "react";

export function MessageInput({
  turnActive,
  onSend,
  onInterrupt,
}: {
  turnActive: boolean;
  onSend: (text: string) => void;
  onInterrupt: () => void;
}) {
  const [text, setText] = useState("");

  const send = () => {
    if (!text.trim()) return;
    onSend(text);
    setText("");
  };

  return (
    <div className="flex items-end gap-2 border-t border-neutral-800 bg-neutral-900 p-3">
      <textarea
        value={text}
        onChange={(e) => setText(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter" && !e.shiftKey) {
            e.preventDefault();
            send();
          }
        }}
        rows={3}
        placeholder="Type a message… (Enter to send, Shift+Enter for newline)"
        className="flex-1 resize-none rounded-md border border-neutral-700 bg-neutral-950 px-3 py-2 text-sm text-neutral-100 placeholder:text-neutral-600 focus:border-neutral-500 focus:outline-none"
      />
      <button
        type="button"
        onClick={turnActive ? onInterrupt : send}
        className={
          turnActive
            ? "rounded-md bg-red-600 px-4 py-2 text-sm font-medium text-white hover:bg-red-500"
            : "rounded-md bg-blue-600 px-4 py-2 text-sm font-medium text-white hover:bg-blue-500"
        }
      >
        {turnActive ? "Stop" : "Send"}
      </button>
    </div>
  );
}
