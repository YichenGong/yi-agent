import type { ReactNode } from "react";
import { useEscapeKey } from "./useEscapeKey";

export interface PopoverProps {
  open: boolean;
  onClose: () => void;
  /**
   * 是否渲染"点外关闭"的透明遮罩。
   *
   * 原实现里锚定下拉用 `fixed inset-0 z-10` + `onClick={close}`；有的下拉
   * （如 `MessageInput` 的 slash 弹窗）不用遮罩，故可关。
   */
  backdrop?: boolean;
  backdropClassName?: string;
  /**
   * 透传到遮罩 `div` 的额外属性。
   *
   * `ThreadSidebar` 的会话菜单遮罩带 `data-thread-menu-overlay=""` 标记（`useLongPress`
   * 的吞 click 白名单，也是其测试的选择器），且它的 onClick 要先 `e.stopPropagation()`
   * 再关闭——故给定 `onClick` 时由调用方自行负责关闭。
   */
  backdropProps?: React.HTMLAttributes<HTMLDivElement> & {
    /**
     * `data-*` 标记（如 `data-thread-menu-overlay=""`）。
     *
     * React 未给 `HTMLAttributes` 索引签名，不加这条则调用点传 `data-*` 会被
     * excess property check 拒掉（TS2353），而该标记正是上面那段注释的用途。
     */
    [dataAttr: `data-${string}`]: string | undefined;
  };
  children: ReactNode;
}

/**
 * 锚定下拉/菜单外壳：可选透明遮罩 + Escape 关闭。
 *
 * 覆盖原先散在 `ModelPicker`/`ModeChip`/`ThreadSidebar` 的
 * `fixed inset-0 z-10` 下拉形态。
 */
export function Popover({
  open,
  onClose,
  backdrop = true,
  backdropClassName = "fixed inset-0 z-10",
  backdropProps,
  children,
}: PopoverProps) {
  useEscapeKey(open, onClose);
  if (!open) return null;
  return (
    <>
      {backdrop ? (
        <div className={backdropClassName} onClick={onClose} {...backdropProps} />
      ) : null}
      {children}
    </>
  );
}
