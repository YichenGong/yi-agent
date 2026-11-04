import { useRef, useState } from "react";
import { formatError, isResultUnknown, UNKNOWN_WRITE_TEXT } from "../lib/errors";
import { Popover } from "./Popover";

/**
 * The board's "＋": put a card into the queue.
 *
 * Enqueueing is rare compared to reading the board, so the two entry points
 * live behind one icon rather than occupying the first screenful. This is the
 * only manual entry point (the model uses the CLI; menus only create and remove
 * boards), so it must stay reachable — just not always visible.
 *
 * **Paths are typed, not picked.** The host bridge exposes seven capabilities
 * (context/request/result/subscribe/openThread/notify/confirm) and file picking
 * is not one of them: a plugin page runs in a WebView, and an `<input
 * type="file">` deliberately withholds the real path. Offering a "choose file"
 * button would therefore be a button that cannot work — worse than asking for
 * the path. The page never reads these files; it hands the strings to the
 * backend, which validates them (`validate_promotion`) exactly as the CLI does.
 */
export function Enqueue({
  enqueue,
  prequeue,
  busy = false,
}: {
  enqueue: (specPath: string, planPath: string) => Promise<void>;
  /** Deliver a title-only card; spec/plan come later. */
  prequeue: (title: string) => Promise<void>;
  busy?: boolean;
}) {
  const [open, setOpen] = useState(false);
  const triggerRef = useRef<HTMLButtonElement | null>(null);
  const [working, setWorking] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [title, setTitle] = useState("");
  const [spec, setSpec] = useState("");
  const [plan, setPlan] = useState("");

  const close = () => {
    setOpen(false);
    // The menu can be opened from the keyboard, so closing must put focus back
    // on the trigger rather than dropping it on <body>.
    triggerRef.current?.focus();
  };

  /** A write whose fate is unknown is reported as such, never retried silently. */
  const report = (e: unknown) => {
    setError(isResultUnknown(e) ? UNKNOWN_WRITE_TEXT : formatError(e));
  };

  const submitPair = async () => {
    // A second click while the first is in flight would deliver the card twice.
    if (working || busy) return;
    const specPath = spec.trim();
    const planPath = plan.trim();
    if (specPath === "" || planPath === "") return;
    setWorking(true);
    setError(null);
    setNotice(null);
    try {
      await enqueue(specPath, planPath);
      // Delivered, not queued: the backend files it on its next tick.
      setNotice("已投递，等待后端校验");
      setSpec("");
      setPlan("");
      close();
    } catch (e) {
      report(e);
    } finally {
      setWorking(false);
    }
  };

  /** Title only. A blank title is ignored here rather than sent. */
  const submitTitle = async () => {
    if (working || busy) return;
    const trimmed = title.trim();
    if (trimmed === "") return;
    setWorking(true);
    setError(null);
    setNotice(null);
    try {
      await prequeue(trimmed);
      setTitle("");
      setNotice("已投递，等待后端校验");
      close();
    } catch (e) {
      report(e);
    } finally {
      setWorking(false);
    }
  };

  const disabled = working || busy;
  return (
    <div className="kanban-enqueue">
      <button
        ref={triggerRef}
        type="button"
        className="kanban-icon-button"
        aria-haspopup="menu"
        aria-expanded={open}
        aria-label="添加到看板"
        title="添加到看板"
        disabled={disabled}
        onClick={() => setOpen((value) => !value)}
      >
        ＋
      </button>
      <Popover open={open} onClose={close}>
        <div role="menu" aria-label="添加到看板" className="kanban-menu">
          <div className="kanban-menu-section" role="group" aria-label="加入看板">
            <span className="kanban-menu-label">加入看板（spec 与 plan 的路径）</span>
            <input
              aria-label="spec 路径"
              value={spec}
              disabled={disabled}
              onChange={(event) => setSpec(event.currentTarget.value)}
              placeholder="/abs/path/to/change.spec.md"
              className="kanban-input"
            />
            <input
              aria-label="plan 路径"
              value={plan}
              disabled={disabled}
              onChange={(event) => setPlan(event.currentTarget.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter") {
                  event.preventDefault();
                  void submitPair();
                }
              }}
              placeholder="/abs/path/to/change.plan.md"
              className="kanban-input"
            />
            <button
              type="button"
              aria-label="加入看板"
              disabled={disabled}
              onClick={() => void submitPair()}
              className="kanban-menu-item"
            >
              加入看板
            </button>
          </div>
          <div className="kanban-menu-row">
            <input
              aria-label="预排队标题"
              value={title}
              disabled={disabled}
              onChange={(event) => setTitle(event.currentTarget.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter") {
                  event.preventDefault();
                  void submitTitle();
                }
              }}
              placeholder="还没写 spec/plan？先记个标题"
              className="kanban-input"
            />
            <button
              type="button"
              aria-label="预排队"
              disabled={disabled}
              onClick={() => void submitTitle()}
              className="kanban-button"
            >
              预排队
            </button>
          </div>
        </div>
      </Popover>
      {notice ? (
        <p className="kanban-notice-inline" role="status">
          {notice}
        </p>
      ) : null}
      {error ? (
        <p className="kanban-error" role="alert">
          {error}
        </p>
      ) : null}
    </div>
  );
}
