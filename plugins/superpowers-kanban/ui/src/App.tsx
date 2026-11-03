import { useCallback, useEffect, useState } from "react";
import type { PluginHostApi } from "@yi-agent/plugin-sdk";
import { BoardView } from "./components/BoardView";
import { SettingsPanel } from "./components/SettingsPanel";
import { useBoardState } from "./hooks/useBoardState";
import { enqueueCard, enqueuePrequeuedCard, promoteCard, writeSwitch } from "./lib/board";
import { DISCONNECTED_NOTICE } from "./host";
import { formatError } from "./lib/errors";

/**
 * The board page. Two views behind one entry point: the board itself and its
 * settings, chosen by the tab strip the host hands us in `PluginContext`.
 *
 * The pending-session link is intentional: opening a card's session is a host
 * capability (`openThread`), not something the plugin could do itself, and it
 * must stay scoped to what the host allows. No thread id is ever constructed
 * here.
 */
export function App({
  host,
  view = "main",
  pollMs,
}: {
  host: PluginHostApi | null;
  /** Which view to render. The host opens `main` and `settings` separately. */
  view?: "main" | "settings";
  /** Poll interval override, for tests. */
  pollMs?: number;
}) {
  const board = useBoardState({ host, pollMs });
  const [doneExpanded, setDoneExpanded] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);

  // Expansion is pure view state and resets whenever the page is reopened: a
  // collapsed done column is the default the user expects each time.
  useEffect(() => {
    setDoneExpanded(false);
  }, [view]);

  /**
   * Run a write, then refresh. The error is rethrown after it is shown because
   * callers need to know whether the write happened: `Enqueue` says "已投递"
   * only when its promise resolves, so a swallowed failure would make a failed
   * write look delivered. Callers that only need the inline message (the switch
   * toggle) mark that with `void`.
   */
  const run = useCallback(
    async (action: (h: PluginHostApi) => Promise<void>) => {
      if (host === null) {
        setActionError(DISCONNECTED_NOTICE);
        throw new Error(DISCONNECTED_NOTICE);
      }
      setActionError(null);
      try {
        await action(host);
        await board.refreshSelectedBoard();
      } catch (error) {
        setActionError(formatError(error));
        throw error;
      }
    },
    [host, board],
  );

  /**
   * Fire-and-forget writes: the inline message is the whole report, and the
   * rejection is already shown by `run`. Without the catch, `run`'s rethrow
   * would surface as an unhandled rejection in the page console for something
   * the user is already looking at.
   */
  const fire = useCallback(
    (action: (h: PluginHostApi) => Promise<void>) => {
      void run(action).catch(() => {});
    },
    [run],
  );

  if (host === null) {
    return (
      <p className="kanban-notice" role="status">
        {DISCONNECTED_NOTICE}
      </p>
    );
  }

  if (view === "settings") {
    return <SettingsPanel host={host} />;
  }

  return (
    <>
      <BoardView
        switchOn={board.boardOn}
        source={board.boardSource}
        cards={board.boardCards}
        pluginMissing={board.boardPluginMissing}
        expandedDone={doneExpanded}
        busy={board.loading}
        error={actionError}
        onToggleDone={() => setDoneExpanded((value) => !value)}
        onOpenThread={(threadId) => void host.openThread(threadId)}
        onPromote={(cardId, specPath, planPath) =>
          run((h) => promoteCard(h, cardId, specPath, planPath))
        }
        onEnqueue={(spec, plan) => run((h) => enqueueCard(h, spec, plan))}
        onPrequeue={(title) => run((h) => enqueuePrequeuedCard(h, title, null))}
        onToggleSwitch={(on) => fire((h) => writeSwitch(h, on))}
      />
      {board.boardError !== null ? (
        <p className="kanban-error" role="alert">
          {board.boardError === "disconnected"
            ? DISCONNECTED_NOTICE
            : "后端未就绪，本次写入没有生效。"}
        </p>
      ) : null}
    </>
  );
}
