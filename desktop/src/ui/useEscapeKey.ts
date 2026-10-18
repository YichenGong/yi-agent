import { useEffect } from "react";

/**
 * 当 `active` 为真时，在 document 上监听 Escape 并调用 `onEscape`。
 *
 * 各调用点原先各写一份监听，但它们的**条件**与**回焦目标**都不同：条件由
 * `active` 表达；回焦由 `onEscape` 自己负责（它通常会把焦点交回触发按钮）。
 */
export function useEscapeKey(active: boolean, onEscape: () => void): void {
  useEffect(() => {
    if (!active) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onEscape();
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [active, onEscape]);
}
