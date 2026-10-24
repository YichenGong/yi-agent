import { useEffect, useRef } from "react";

export type SearchBarProps = {
  /** 当前输入（受控）。 */
  value: string;
  /** 命中总数。 */
  total: number;
  /** 当前命中下标，0 基。 */
  active: number;
  onChange: (value: string) => void;
  onNext: () => void;
  onPrev: () => void;
  onClose: () => void;
};

/**
 * 会话内查找条。纯受控组件：它不认识转录，只把用户意图报上去。
 *
 * 打开即落焦并全选输入框（再次 Cmd+F 时直接改写上次的词）。Enter / Shift+Enter
 * 上下跳；Esc **不在这里处理**——由宿主的 `useEscapeKey` 统一管，避免与审批
 * 弹窗等其它 Esc 语义各写一份监听。
 */
export function SearchBar({
  value,
  total,
  active,
  onChange,
  onNext,
  onPrev,
  onClose,
}: SearchBarProps) {
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    const el = inputRef.current;
    if (!el) return;
    el.focus();
    el.select();
  }, []);

  const hasHits = total > 0;

  return (
    <div
      role="search"
      aria-label="会话内搜索"
      className="absolute right-3 top-3 z-20 flex items-center gap-2 rounded-md border border-line-strong bg-panel px-2 py-1 shadow-lg"
    >
      <input
        ref={inputRef}
        type="search"
        role="searchbox"
        aria-label="查找文本"
        value={value}
        placeholder="在会话中查找"
        onChange={(e) => onChange(e.target.value)}
        onKeyDown={(e) => {
          if (e.key !== "Enter") return;
          // IME 确认键（中文输入法回车上屏）不是导航。
          if (e.nativeEvent.isComposing || e.keyCode === 229) return;
          e.preventDefault();
          if (e.shiftKey) onPrev();
          else onNext();
        }}
        className="w-40 bg-transparent text-sm text-fg outline-none placeholder:text-fg-subtle"
      />
      <span
        aria-live="polite"
        className={hasHits ? "shrink-0 text-xs text-fg-muted" : "shrink-0 text-xs text-amber-400"}
      >
        {hasHits ? `${active + 1}/${total}` : value.trim() === "" ? "" : "无结果"}
      </span>
      <button
        type="button"
        aria-label="上一个"
        disabled={!hasHits}
        onClick={onPrev}
        className="rounded px-1 text-fg-muted hover:text-fg disabled:opacity-40"
      >
        ↑
      </button>
      <button
        type="button"
        aria-label="下一个"
        disabled={!hasHits}
        onClick={onNext}
        className="rounded px-1 text-fg-muted hover:text-fg disabled:opacity-40"
      >
        ↓
      </button>
      <button
        type="button"
        aria-label="关闭查找"
        onClick={onClose}
        className="rounded px-1 text-fg-muted hover:text-fg"
      >
        ✕
      </button>
    </div>
  );
}
