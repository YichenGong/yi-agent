/**
 * The collapsed Superpowers 看板 panel.
 *
 * The board column hands its whole width back to the chat area when collapsed;
 * this strip keeps the panel identifiable (same accessible name) and carries the
 * single affordance needed to bring it back. Collapse and expand have to exist in
 * pairs - the sibling 「子 agent」 rail collapses with no way to reopen, and this
 * strip is what keeps the board from repeating that.
 */
export function SuperpowersKanbanCollapsedStrip({
  onExpand,
}: {
  onExpand: () => void;
}) {
  return (
    <aside
      aria-label="Superpowers 看板"
      className="flex w-8 shrink-0 flex-col items-center border-r border-line bg-raised py-3"
    >
      <button
        type="button"
        aria-label="展开看板"
        className="text-xs text-fg-subtle hover:text-fg-muted"
        onClick={onExpand}
        title="展开看板"
      >
        看板
      </button>
    </aside>
  );
}
