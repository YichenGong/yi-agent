import { useRef, useState } from "react";
import { formatError } from "../lib/errorMessage";
import { Popover } from "../ui/Popover";

/**
 * 看板顶栏的「＋」：把卡片送进队列。
 *
 * 原来这里是一条常驻的「加入看板」全宽按钮 + 一个预排队输入框，占掉看板首屏
 * 一整块。入队是低频动作，而卡片列表才是主要内容，故收进一个「＋」弹出菜单：
 * 平时只占一个图标位，点开才出现两条入口。
 *
 * 功能一个没少——这是用户唯一的手工入队入口（右键菜单只有创建/移除看板，
 * 模型侧走 CLI），所以它不能删，只能收起来。
 *
 * 选择与投递都由宿主注入，组件自己不碰 Tauri 与 RPC：测试直接驱动桩选择器。
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
  const [open, setOpen] = useState(false);
  const triggerRef = useRef<HTMLButtonElement | null>(null);
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [title, setTitle] = useState("");

  const close = () => {
    setOpen(false);
    // 焦点交回触发按钮：菜单是用键盘开出来的，关掉后焦点不能丢在 body 上。
    triggerRef.current?.focus();
  };

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
      close();
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
      close();
    } catch (e) {
      setError(formatError(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="relative inline-block">
      <button
        ref={triggerRef}
        type="button"
        aria-haspopup="menu"
        aria-expanded={open}
        aria-label="添加到看板"
        title="添加到看板"
        disabled={busy}
        onClick={() => setOpen((v) => !v)}
        className="flex size-6 items-center justify-center rounded border border-line-strong text-xs text-fg-muted hover:text-fg disabled:opacity-50"
      >
        ＋
      </button>
      <Popover open={open} onClose={close}>
        <div
          role="menu"
          aria-label="添加到看板"
          className="absolute right-0 top-full z-20 mt-1 w-64 rounded-md border border-line-strong bg-raised p-2 shadow-xl"
        >
          <button
            type="button"
            role="menuitem"
            aria-label="加入看板"
            disabled={busy}
            onClick={() => void onEnqueue()}
            className="w-full rounded px-2 py-1.5 text-left text-xs text-fg hover:bg-panel disabled:opacity-50"
          >
            加入看板（选 spec 与 plan）
          </button>
          <div className="mt-2 flex items-center gap-1">
            <input
              aria-label="预排队标题"
              value={title}
              disabled={busy}
              onChange={(e) => setTitle(e.currentTarget.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") {
                  e.preventDefault();
                  void onPrequeue();
                }
              }}
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
        </div>
      </Popover>
      {notice ? <p className="mt-2 text-xs text-fg-muted">{notice}</p> : null}
      {error ? <p className="mt-2 text-xs text-red-400">{error}</p> : null}
    </div>
  );
}
