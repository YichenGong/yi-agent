import { useState } from "react";
import { formatError } from "../lib/errorMessage";

/**
 * The board panel's "add a card" control.
 *
 * Picking and enqueuing are injected so the component itself never reaches for
 * Tauri or the RPC client: the host owns those, and the tests here drive a stub
 * picker instead of mocking the OS dialog.
 */
export function SuperpowersKanbanEnqueue({
  pickFile,
  enqueue,
}: {
  /** Returns the chosen path, or null when the user cancels. */
  pickFile: () => Promise<string | null>;
  enqueue: (specPath: string, planPath: string) => Promise<void>;
}) {
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const onEnqueue = async () => {
    // A second click while the first is in flight would deliver the card twice.
    if (busy) return;
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const spec = await pickFile();
      if (spec === null) return;
      const plan = await pickFile();
      if (plan === null) return;
      await enqueue(spec, plan);
      // Delivered, not queued: the plugin files it on its next tick.
      setNotice("已投递，等待插件校验");
    } catch (e) {
      setError(formatError(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="px-4 pb-3">
      <button
        type="button"
        aria-label="加入看板"
        disabled={busy}
        onClick={() => void onEnqueue()}
        className="w-full rounded border border-line-strong px-2 py-1 text-xs text-fg-muted hover:text-fg disabled:opacity-50"
      >
        加入看板
      </button>
      {notice ? <p className="mt-2 text-xs text-fg-muted">{notice}</p> : null}
      {error ? <p className="mt-2 text-xs text-red-400">{error}</p> : null}
    </div>
  );
}
