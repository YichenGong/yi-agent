import { useEffect, useRef, useState } from "react";

/**
 * The throttle window for streamed text, in ms.
 *
 * ~4 frames at 60Hz: long enough that a token-per-delta firehose collapses into
 * a few re-parses per second, short enough that typing-speed text still looks
 * live. Only the *streaming* message is throttled; settled messages are memoized
 * on their text and cost nothing either way.
 */
export const THROTTLE_MS = 60;

/**
 * Throttle `text` to at most one update per `ms`. The **first** change after an
 * idle period renders immediately (leading edge, so the reply starts appearing
 * at once rather than after a whole silent window); subsequent changes inside
 * the same window collapse into a single **trailing** flush that renders the
 * *last* value, never the first.
 *
 * The last value is what must never be lost: a purely leading throttle would
 * show the first token of a window and then hold it, so the visible text would
 * lag the stream by a full window for the rest of the turn.
 *
 * The final value is returned eagerly whenever `text` stops changing between
 * renders (the caller has just seen the settled text — e.g. `item/completed`
 * replaced the whole item), so a window can never swallow the authoritative
 * text.
 */
export function useThrottledText(text: string, ms: number = THROTTLE_MS): string {
  const [shown, setShown] = useState(text);
  const lastFlush = useRef(0);
  const pending = useRef(text);

  useEffect(() => {
    pending.current = text;
    if (text === shown) return;

    const now = Date.now();
    const elapsed = now - lastFlush.current;
    if (elapsed >= ms) {
      lastFlush.current = now;
      setShown(text);
      return;
    }

    const timer = window.setTimeout(() => {
      lastFlush.current = Date.now();
      // Trailing edge: flush whatever the latest value is, not the one that
      // arrived when the timer was scheduled.
      setShown(pending.current);
    }, ms - elapsed);
    return () => window.clearTimeout(timer);
  }, [text, ms, shown]);

  return shown;
}
