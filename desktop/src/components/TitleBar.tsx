/**
 * Full-width title-bar strip for the macOS Overlay window.
 *
 * With `titleBarStyle: "Overlay"` the native title bar becomes a transparent
 * layer over the content, so this element supplies both the surface behind the
 * traffic lights and the drag region that moves the window. It deliberately
 * reuses the sidebar's surface color (`bg-neutral-900` + `border-b
 * border-neutral-800`) so the top strip and the sidebar read as one surface.
 */
export function TitleBar() {
  return (
    <div
      data-tauri-drag-region
      className="h-8 shrink-0 border-b border-neutral-800 bg-neutral-900"
    />
  );
}
