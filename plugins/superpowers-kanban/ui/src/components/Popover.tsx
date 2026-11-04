import { useEffect, type ReactNode } from "react";

export interface PopoverProps {
  open: boolean;
  onClose: () => void;
  /** Render a click-catching backdrop. Menus that stay anchored often do not want one. */
  backdrop?: boolean;
  backdropClassName?: string;
  children: ReactNode;
}

/**
 * An anchored menu shell: optional transparent backdrop plus Escape-to-close.
 *
 * Deliberately self-contained. The desktop app had a shared `Popover` wired to
 * its own Escape hook and Tailwind classes; a plugin view cannot depend on
 * either, because the host's CSS and modules are not part of the plugin's
 * bundle. Owning the behaviour here also means the keyboard contract is tested
 * by the plugin rather than inherited by accident.
 */
export function Popover({
  open,
  onClose,
  backdrop = true,
  backdropClassName = "kanban-popover-backdrop",
  children,
}: PopoverProps) {
  // Listen only while open, and re-register when `onClose` changes identity so
  // Escape never fires a stale handler.
  useEffect(() => {
    if (!open) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.stopPropagation();
        onClose();
      }
    };
    document.addEventListener("keydown", onKeyDown);
    return () => document.removeEventListener("keydown", onKeyDown);
  }, [open, onClose]);

  if (!open) return null;
  return (
    <>
      {backdrop ? <div className={backdropClassName} onClick={onClose} aria-hidden="true" /> : null}
      {children}
    </>
  );
}
