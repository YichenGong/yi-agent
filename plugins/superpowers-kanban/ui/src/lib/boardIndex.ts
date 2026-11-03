/**
 * The board's summary line: how much work is queued, in five counts.
 *
 * Only cards that are still on their way somewhere are counted. Pre-queued
 * cards wait for earlier work, queued cards wait for a slot, running cards hold
 * one, awaiting-merge cards wait for the user, and needs-you cards wait for an
 * answer. Merging cards were once invisible here — five stuck cards still read
 * "0 queued · 0 running" — and a board holding only pre-queued cards read "no
 * work". Completion, failure and cancellation are history and stay out.
 */
export function summarize(cards: { state: string }[]): string {
  if (cards.length === 0) return "空";
  let preQueued = 0;
  let queued = 0;
  let active = 0;
  let awaitingMerge = 0;
  let mergeQueued = 0;
  let needsYou = 0;
  for (const card of cards) {
    const state = card.state.toLowerCase();
    if (state === "pre_queued") preQueued += 1;
    else if (state === "queued" || state === "launching") queued += 1;
    else if (state === "running" || state === "merging") active += 1;
    else if (state === "awaiting_merge") awaitingMerge += 1;
    // Confirmed for merge, waiting for a slot: the user has nothing left to do,
    // but the card must not vanish from the summary either.
    else if (state === "merge_queued") mergeQueued += 1;
    else if (state === "needs_you") needsYou += 1;
  }
  const parts: string[] = [];
  // Pre-queued first, matching the leftmost column on the board.
  if (preQueued > 0) parts.push(`${preQueued} 预排队`);
  if (queued > 0) parts.push(`${queued} 排队`);
  if (active > 0) parts.push(`${active} 运行中`);
  if (awaitingMerge > 0) parts.push(`${awaitingMerge} 待合并`);
  if (mergeQueued > 0) parts.push(`${mergeQueued} 合并排队中`);
  if (needsYou > 0) parts.push(`${needsYou} 待处理`);
  return parts.length > 0 ? parts.join(" · ") : "无待办";
}
