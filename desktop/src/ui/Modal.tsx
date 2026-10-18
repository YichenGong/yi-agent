import { useEffect, useRef, type ReactNode, type RefObject } from "react";
import { useEscapeKey } from "./useEscapeKey";

export interface ModalProps {
  open: boolean;
  onClose: () => void;
  /** 关闭后焦点交回的控件。 */
  restoreFocusTo?: RefObject<HTMLElement | null>;
  /** 遮罩类名（各弹层外观不同，故由调用方传入原类名，保证 DOM 不变）。 */
  overlayClassName?: string;
  /** 面板类名。 */
  panelClassName?: string;
  labelledBy?: string;
  /** 面板的可访问名，与 `labelledBy` 二选一（原调用点两者各用其一）。 */
  ariaLabel?: string;
  /** 渲染在遮罩内、面板前的额外内容（如 `SettingsDialog` 自己的 `[data-settings-backdrop]` 层）。 */
  overlayBackdrop?: ReactNode;
  children: ReactNode;
}

const DEFAULT_OVERLAY = "fixed inset-0 z-50 flex items-center justify-center";

/**
 * 弹层外壳：遮罩 + `role="dialog"` + Escape 关闭 + 焦点还原。
 *
 * 这几样原先在 `SettingsDialog`/`ModeChip`/`OnboardingWizard`/`PairingScreen`
 * 各写了一遍。
 */
export function Modal({
  open,
  onClose,
  restoreFocusTo,
  overlayClassName = DEFAULT_OVERLAY,
  panelClassName,
  labelledBy,
  ariaLabel,
  overlayBackdrop,
  children,
}: ModalProps) {
  useEscapeKey(open, onClose);

  // 只在「曾经打开过、现在关上了」时回焦：初次挂载（open=false）不得抢焦点。
  const wasOpen = useRef(false);
  useEffect(() => {
    if (open) {
      wasOpen.current = true;
      return;
    }
    if (wasOpen.current) {
      wasOpen.current = false;
      restoreFocusTo?.current?.focus();
    }
  }, [open, restoreFocusTo]);

  if (!open) return null;
  return (
    <div className={overlayClassName}>
      {overlayBackdrop}
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby={labelledBy}
        aria-label={ariaLabel}
        className={panelClassName}
      >
        {children}
      </div>
    </div>
  );
}
