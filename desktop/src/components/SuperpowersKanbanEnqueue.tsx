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
  prequeue,
}: {
  /** Returns the chosen path, or null when the user cancels. */
  pickFile: () => Promise<string | null>;
  enqueue: (specPath: string, planPath: string) => Promise<void>;
  /** 投递一张只有标题的预排队卡；spec/plan 以后再补。 */
  prequeue: (title: string) => Promise<void>;
}) {
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [title, setTitle] = useState("");

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

  /**
   * 只投递标题。空标题（含纯空白）就地忽略：插件也会拒，但没必要发这趟。
   */
  const onPrequeue = async () => {
    if (busy) return;
    const trimmed = title.trim();
    if (trimmed === "") return;
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      await prequeue(trimmed);
      setTitle("");
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
      <div className="mt-2 flex items-center gap-1">
        <input
          aria-label="预排队标题"
          value={title}
          disabled={busy}
          onChange={(e) => setTitle(e.currentTarget.value)}
          placeholder="还没写 spec/plan？先记个标题"
          className="min-w-0 flex-1 rounded border border-line bg-panel px-2 py-1 text-xs text-fg"
        />
        <button
          type="button"
          aria-label="预排队"
          disabled={busy}
          onClick={() => void onPrequeue()}
          className="shrink-0 cursor-pointer rounded border border-line-strong px-2 py-1 text-xs text-fg-muted hover:text-fg disabled:opacity-50"
        >
          预排队
        </button>
      </div>
      {notice ? <p className="mt-2 text-xs text-fg-muted">{notice}</p> : null}
      {error ? <p className="mt-2 text-xs text-red-400">{error}</p> : null}
    </div>
  );
}
