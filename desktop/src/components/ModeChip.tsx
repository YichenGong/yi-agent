import { useCallback, useEffect, useId, useState } from "react";
import type { ThreadMode } from "../lib/threadPermissionMode";

const LABEL: Record<ThreadMode, string> = { normal: "Normal", yolo: "YOLO" };

/** Per-thread permission-mode chip. Selecting YOLO asks for confirmation first;
 *  going back to Normal applies immediately (relaxing is what needs a gate). */
export function ModeChip({
  mode,
  onChange,
  disabled,
}: {
  mode: ThreadMode;
  onChange: (mode: ThreadMode) => void;
  disabled?: boolean;
}) {
  const [open, setOpen] = useState(false);
  const [confirming, setConfirming] = useState(false);
  const headingId = useId();

  const close = useCallback(() => {
    setOpen(false);
    setConfirming(false);
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

  const isYolo = mode === "yolo";

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

  return (
    <div className="relative inline-block">
      <button
        type="button"
        aria-haspopup="menu"
        aria-expanded={open}
        aria-label={`Permission mode: ${LABEL[mode]}`}
        disabled={disabled}
        onClick={() => setOpen((v) => !v)}
        className={`rounded-full border px-2 py-0.5 text-xs font-medium disabled:opacity-50 ${
          isYolo
            ? "border-red-600 bg-red-600 text-white hover:bg-red-500"
            : "border-neutral-700 text-neutral-400 hover:bg-neutral-800"
        }`}
      >
        {LABEL[mode]}
      </button>

      {open && (
        <>
          <div className="fixed inset-0 z-10" onClick={close} />
          <div
            role="menu"
            className="absolute bottom-full left-0 z-20 mb-1 rounded-md border border-neutral-700 bg-neutral-800 py-1 shadow-xl"
          >
            {(["normal", "yolo"] as const).map((m) => (
              <button
                key={m}
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
                type="button"
                onClick={close}
                className="rounded-md border border-neutral-600 px-3 py-1.5 text-sm text-neutral-200 hover:bg-neutral-800"
              >
                Cancel
              </button>
              <button
                type="button"
                onClick={() => {
                  setConfirming(false);
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
