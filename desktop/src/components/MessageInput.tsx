import { useEffect, useRef, useState } from "react";
import { ModeChip } from "./ModeChip";
import { SlashPopup } from "./SlashPopup";
import { filterCommands, parseSlashInput } from "../lib/slash";
import { isImeEnter, useImeGuard } from "../lib/imeEnter";
import type { ThreadMode } from "../lib/threadPermissionMode";

export function MessageInput({
  turnActive,
  onSend,
  onInterrupt,
  mode,
  onModeChange,
  onSlashCommand,
}: {
  turnActive: boolean;
  onSend: (text: string) => Promise<boolean>;
  onInterrupt: () => void;
  mode: ThreadMode | null;
  onModeChange: (mode: ThreadMode) => void;
  onSlashCommand: (name: string, args: string | null) => void;
}) {
  const [text, setText] = useState("");
  const [sending, setSending] = useState(false);
  // Latch: whether the popup should currently be offered. Escape clears it to
  // dismiss; typing re-arms it from the text's own shape (see onChange).
  const [popupOpen, setPopupOpen] = useState(false);
  const [selected, setSelected] = useState(0);
  const inputRef = useRef<HTMLTextAreaElement>(null);
  // Enter that confirms an IME candidate must not send the message; see
  // `isImeCompositionKey` for why keyCode 229 is the load-bearing check here.
  const ime = useImeGuard();

  const parsed = parseSlashInput(text);
  // Command names contain no spaces (the catalog has no argument-bearing
  // names), so a space means the caret left the name for the argument list and
  // the popup closes. A bare `/` opens the popup too, even though
  // `parseSlashInput("/")` is `{ kind: "none" }` — a lone slash names no
  // command but is still the moment the user asked for the menu.
  const typingName = /^\/[^\s/]*$/.test(text.trim());
  const showPopup = popupOpen && typingName;
  const options = showPopup ? filterCommands(parsed.kind === "command" ? parsed.name : "") : [];

  useEffect(() => {
    setSelected(0);
  }, [text]);

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

  /** Run the slash command the input currently names (or report it unknown). */
  const runSlash = (name: string, args: string | null) => {
    onSlashCommand(name, args);
    setText("");
    setPopupOpen(false);
  };

  return (
    <div className="relative flex items-end gap-2 border-t border-line bg-panel p-3">
      {showPopup && <SlashPopup commands={options} selected={selected} />}
      <textarea
        ref={inputRef}
        value={text}
        onChange={(e) => {
          const next = e.target.value;
          setText(next);
          // Re-arm the popup whenever the text still looks like a command name.
          setPopupOpen(/^\/[^\s/]*$/.test(next.trim()));
        }}
        onCompositionStart={ime.onCompositionStart}
        onCompositionEnd={ime.onCompositionEnd}
        onBlur={ime.resetComposition}
        onKeyDown={(e) => {
          if (isImeEnter(e, ime.composing.current)) return;
          if (showPopup && options.length > 0) {
            if (e.key === "ArrowDown") {
              e.preventDefault();
              setSelected((i) => (i + 1) % options.length);
              return;
            }
            if (e.key === "ArrowUp") {
              e.preventDefault();
              setSelected((i) => (i - 1 + options.length) % options.length);
              return;
            }
            if (e.key === "Tab") {
              e.preventDefault();
              const picked = options[Math.min(selected, options.length - 1)];
              setText(`/${picked.name} `);
              setPopupOpen(false);
              inputRef.current?.focus();
              return;
            }
            if (e.key === "Escape") {
              e.preventDefault();
              setPopupOpen(false);
              return;
            }
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              // A bare "/" names no command (`parseSlashInput("/")` is
              // `{ kind: "none" }`), yet the popup is offered for it and its
              // DEFAULT highlight is index 0 — the destructive `/clear`. A stray
              // Enter on that untouched default would erase the transcript, so it
              // is inert until the user either types a name character (kind turns
              // "command", as for "/cos") or moves the highlight with ↑/↓ (an
              // explicit choice, which the line below honours). Tab still
              // completes.
              if (parsed.kind !== "command" && selected === 0) return;
              // A space closes the popup (name-mode only), so no arguments can
              // be pending here: accepting the highlighted row is exactly what
              // "complete and run" means (`/cos` -> `/cost`). Fully typed
              // commands — arguments and unknowns included — reach the
              // no-popup branch below with `parsed` intact.
              const picked = options[Math.min(selected, options.length - 1)];
              runSlash(picked.name, null);
              return;
            }
            return; // 弹窗开启时吞掉其余按键,不作文本处理
          }
          if (e.key === "Escape" && popupOpen) {
            e.preventDefault();
            setPopupOpen(false);
            return;
          }
          if (e.key === "Enter" && !e.shiftKey) {
            // Confirming a candidate with Enter is the IME's key, not the user's:
            // let the composition land in the box and wait for the next Enter.
            e.preventDefault();
            if (parsed.kind === "command") {
              // This branch is reached when the popup matched nothing: unknown
              // commands and matched commands alike belong to the command
              // layer, never to the agent (`/nope` reports, it does not send).
              runSlash(parsed.name, parsed.args);
              return;
            }
            if (parsed.kind === "path") {
              // Two-slash first token is a path (TUI parity) — falls through.
            } else if (turnActive) {
              onInterrupt();
              return;
            }
            void handleSend();
          }
        }}
        disabled={sending || turnActive}
        rows={3}
        placeholder="Type a message… (Enter to send, Shift+Enter for newline)"
        className="flex-1 resize-none rounded-md border border-line-strong bg-surface px-3 py-2 text-sm text-fg placeholder:text-fg-faint focus:border-fg-subtle focus:outline-none disabled:opacity-50"
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
