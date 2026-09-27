import {
  useCallback,
  useEffect,
  useId,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
} from "react";
import type { ThreadMode } from "../lib/threadPermissionMode";

const LABEL: Record<ThreadMode, string> = { normal: "Normal", yolo: "YOLO" };
const OPTIONS = ["normal", "yolo"] as const;

/** Per-thread permission-mode chip. Selecting YOLO asks for confirmation first;
 *  going back to Normal applies immediately (relaxing is what needs a gate). */
export function ModeChip({
  mode,
  onChange,
  disabled,
}: {
  mode: ThreadMode | null;
  onChange: (mode: ThreadMode) => void;
  disabled?: boolean;
}) {
  const [open, setOpen] = useState(false);
  const [confirming, setConfirming] = useState(false);
  const headingId = useId();

  const triggerRef = useRef<HTMLButtonElement>(null);
  // Focus starts on the safe action so a reflexive Enter can't enable YOLO.
  const cancelRef = useRef<HTMLButtonElement>(null);
  const itemRefs = useRef<(HTMLButtonElement | null)[]>([]);

  const close = useCallback(() => {
    setOpen(false);
    setConfirming(false);
    // Single place that hands focus back once a surface is dismissed.
    triggerRef.current?.focus();
  }, []);

  // Escape dismisses whichever surface is open without changing the mode.
  useEffect(() => {
    if (!open && !confirming) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        close();
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [open, confirming, close]);

  // Move focus into whichever surface just opened.
  useEffect(() => {
    if (!open) return;
    const idx = mode === null ? 0 : OPTIONS.indexOf(mode);
    itemRefs.current[idx]?.focus();
  }, [open, mode]);

  useEffect(() => {
    if (confirming) cancelRef.current?.focus();
  }, [confirming]);

  const isYolo = mode === "yolo";
  // An unknown mode (no thread / failed lookup) must not masquerade as "Normal".
  const label = mode === null ? "Mode" : LABEL[mode];

  const choose = (next: ThreadMode) => {
    if (next === mode) {
      setOpen(false);
      return;
    }
    if (next === "yolo") {
      setOpen(false);
      setConfirming(true);
      return;
    }
    setOpen(false);
    onChange("normal");
  };

  const moveFocus = (dir: 1 | -1) => {
    const items = itemRefs.current.filter(
      (el): el is HTMLButtonElement => el != null,
    );
    if (items.length === 0) return;
    const current = items.findIndex((el) => el === document.activeElement);
    const base = current === -1 ? (dir === 1 ? -1 : 0) : current;
    const next = (base + dir + items.length) % items.length;
    items[next]?.focus();
  };

  const onMenuKeyDown = (e: ReactKeyboardEvent<HTMLDivElement>) => {
    if (e.key === "ArrowDown") {
      e.preventDefault();
      moveFocus(1);
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      moveFocus(-1);
    } else if (e.key === "Tab") {
      // Dismiss, then let focus move on naturally.
      close();
    }
  };

  return (
    <div className="relative inline-block">
      <button
        ref={triggerRef}
        type="button"
        aria-haspopup="menu"
        aria-expanded={open}
        aria-label={`Permission mode: ${mode === null ? "unknown" : LABEL[mode]}`}
        disabled={disabled}
        onClick={() => {
          if (!confirming) setOpen((v) => !v);
        }}
        className={`rounded-full border px-2 py-0.5 text-xs font-medium disabled:opacity-50 ${
          isYolo
            ? "border-red-600 bg-red-600 text-white hover:bg-red-500"
            : "border-neutral-700 text-neutral-400 hover:bg-neutral-800"
        }`}
      >
        {label}
      </button>

      {open && !confirming && (
        <>
          <div className="fixed inset-0 z-10" onClick={close} />
          <div
            role="menu"
            onKeyDown={onMenuKeyDown}
            className="absolute bottom-full left-0 z-20 mb-1 rounded-md border border-neutral-700 bg-neutral-800 py-1 shadow-xl"
          >
            {OPTIONS.map((m, i) => (
              <button
                key={m}
                ref={(el) => {
                  itemRefs.current[i] = el;
                }}
                type="button"
                role="menuitemradio"
                aria-checked={m === mode}
                onClick={() => choose(m)}
                className="flex w-full items-center gap-2 px-3 py-1.5 text-left text-xs whitespace-nowrap text-neutral-200 hover:bg-neutral-700"
              >
                <span className="w-3 text-neutral-400">{m === mode ? "✓" : ""}</span>
                {LABEL[m]}
              </button>
            ))}
          </div>
        </>
      )}

      {confirming && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm">
          <div
            role="dialog"
            aria-modal="true"
            aria-labelledby={headingId}
            className="w-full max-w-md rounded-lg border border-neutral-700 bg-neutral-900 p-4 shadow-xl"
          >
            <h2 id={headingId} className="mb-2 text-sm font-semibold text-neutral-100">
              Enable YOLO mode?
            </h2>
            <ul className="mb-4 list-disc space-y-1 pl-5 text-xs text-neutral-400">
              <li>Skips tool-approval prompts.</li>
              <li>Relaxes the OS sandbox to full access.</li>
              <li>Blacklisted commands are still hard-denied.</li>
            </ul>
            <div className="flex justify-end gap-2">
              <button
                ref={cancelRef}
                type="button"
                onClick={close}
                className="rounded-md border border-neutral-600 px-3 py-1.5 text-sm text-neutral-200 hover:bg-neutral-800"
              >
                Cancel
              </button>
              <button
                type="button"
                onClick={() => {
                  close();
                  onChange("yolo");
                }}
                className="rounded-md bg-red-600 px-3 py-1.5 text-sm font-medium text-white hover:bg-red-500"
              >
                Confirm YOLO
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
