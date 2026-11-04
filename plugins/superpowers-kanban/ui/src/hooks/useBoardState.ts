import { useCallback, useEffect, useRef, useState } from "react";
import type { PluginHostApi } from "@yi-agent/plugin-sdk";
import { makeBoardTick } from "../lib/refresh";
import { normalizeCard, type BoardCard } from "../lib/cards";
import { listCards, readSwitch, type SwitchSource } from "../lib/board";
import { errorKind, type BoardErrorKind } from "../lib/errors";

/**
 * Board concern: the cards on the main view plus the 2-second refresh that
 * keeps them honest.
 *
 * Everything here answers "which project is this board about" — and inside a
 * plugin view that question is already settled by the host: the page is bound
 * to one project handle at handshake, and `host.request` routes to that
 * project's backend. So there is no `project` parameter anywhere in this hook.
 * That is the whole point of moving the UI out: the desktop app used to have to
 * carry `selectedBoard` around and thread it into every call, and every place
 * it forgot produced the "clicked and nothing happened" bug.
 *
 * No request is issued while `host` is null: an unconnected page has no channel
 * to the backend, and firing calls at a missing channel would surface as
 * timeouts rather than as "not connected yet".
 */
export function useBoardState({
  host,
  enabled = true,
  pollMs = 2000,
}: {
  /** The host bridge, or null until the handshake completes. */
  host: PluginHostApi | null;
  /** Whether the plugin is switched on at all. Off means: send nothing. */
  enabled?: boolean;
  /**
   * Poll interval in milliseconds. Tests pass a large value so a mounted
   * component does not race its own assertions, and so "how often we read" is
   * a visible decision rather than a constant buried in the view.
   */
  pollMs?: number;
}) {
  // The guard reads the *latest* enabled value, not the one captured in a
  // closure: the handshake flips `enabled` to true and that same render's
  // callbacks still hold `false`. Guarding on the closure value would drop the
  // one legitimate read at handshake time.
  const enabledRef = useRef(enabled);
  enabledRef.current = enabled;

  const [boardOn, setBoardOn] = useState(false);
  const [boardSource, setBoardSource] = useState<SwitchSource>("default");
  const [boardCards, setBoardCards] = useState<BoardCard[]>([]);
  // The backend answers every board question, so no answer means no backend.
  // Distinct from "there is a backend and its board is empty".
  const [boardPluginMissing, setBoardPluginMissing] = useState(false);
  // The last failed *write*: non-null puts "why it did not take" on the panel
  // instead of failing silently. Only the next successful write or a mount
  // clears it — a successful poll must not erase a failure the user just saw.
  const [boardError, setBoardError] = useState<BoardErrorKind | null>(null);
  const [loading, setLoading] = useState(false);

  /** Read sequence: lets a read that started before a switch recognise itself as stale. */
  const boardSeq = useRef(0);
  const tick = useRef<() => void>(() => {});

  /** Read the board once. Failure here is routine (backend restarting) and silent. */
  const refreshSelectedBoard = useCallback(async () => {
    if (!enabledRef.current || host === null) {
      setBoardCards([]);
      setBoardPluginMissing(false);
      return;
    }
    const seq = ++boardSeq.current;
    try {
      const [sw, cards] = await Promise.all([readSwitch(host), listCards(host)]);
      if (seq !== boardSeq.current) return;
      setBoardOn(sw.on);
      setBoardSource(sw.source);
      setBoardCards(
        cards.map(normalizeCard).filter((card): card is BoardCard => card !== null),
      );
      setBoardPluginMissing(false);
    } catch (error) {
      if (seq !== boardSeq.current) return;
      // "The plugin is not there" is worth naming: it means no backend at all,
      // not "installed and empty". A plain disconnect stays silent — the host
      // will re-handshake and the next poll recovers on its own.
      if (errorKind(error) === "plugin_missing") setBoardPluginMissing(true);
    }
  }, [host]);

  useEffect(() => {
    if (!enabled || host === null) return;
    tick.current = makeBoardTick(refreshSelectedBoard);
    // Read immediately instead of waiting for the first tick: otherwise opening
    // the board shows an empty panel for up to a poll interval.
    void refreshSelectedBoard();
    const timer = setInterval(() => tick.current(), pollMs);
    return () => clearInterval(timer);
  }, [refreshSelectedBoard, enabled, host, pollMs]);

  /** Run a write and record its failure on the panel. */
  const write = useCallback(
    async (action: (host: PluginHostApi) => Promise<void>): Promise<void> => {
      if (host === null) {
        setBoardError("disconnected");
        return;
      }
      setLoading(true);
      setBoardError(null);
      try {
        await action(host);
      } catch (error) {
        setBoardError(errorKind(error));
        throw error;
      } finally {
        setLoading(false);
      }
      // Read back rather than patching local state: the backend owns ordering,
      // ids and normalization, and guessing them here would fork the view from
      // the truth on disk.
      await refreshSelectedBoard();
    },
    [host, refreshSelectedBoard],
  );

  return {
    boardOn,
    boardSource,
    boardCards,
    boardPluginMissing,
    boardError,
    loading,
    setBoardError,
    refreshSelectedBoard,
    write,
  };
}
