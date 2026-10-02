/**
 * The bar that shows a collapsed Superpowers board and offers to open it again.
 *
 * A collapsed board is not deleted: it keeps polling and keeps its state, it is
 * only not drawn. Without this bar there would be no way back — the settings
 * gear does not know about a board, and the sidebar entry merely re-selects the
 * same project, which would be a no-op while that board is the selected one.
 */
export function SuperpowersKanbanCollapsedBar({
  board,
  onExpand,
}: {
  board: string;
  onExpand: () => void;
}) {
  return (
    <div className="flex items-center gap-2 border-b border-neutral-800 bg-neutral-925 px-3 py-1.5 text-xs text-neutral-400">
      <button
        type="button"
        aria-label="展开看板"
        className="text-neutral-400 hover:text-neutral-100"
        onClick={onExpand}
      >
        ▸
      </button>
      <span className="truncate" title={board}>
        Superpowers 看板已收起（{board}）
      </span>
    </div>
  );
}
