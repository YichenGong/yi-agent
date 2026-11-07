/**
 * The one stateful object the subagent trace panel talks to.
 *
 * It owns exactly two machines (`TraceSubscription` and `TraceActions`) and adds
 * only what neither can know on its own:
 *
 * * **Where a request may go.** A connection is registered per slot but starts
 *   *not ready*: registering is the App telling us a client exists; the handshake
 *   is what makes it usable. Nothing requests, retries or subscribes before that,
 *   because a client that has not finished its handshake only produces failures.
 * * **Which source an event came from.** `connectionFor` answers by object
 *   identity, never by "the currently active slot". An event that arrives for a
 *   client we do not know is dropped, not attributed to whatever is on screen.
 * * **One React view.** The two machines emit separately; the view is rebuilt only
 *   when something a reader can see actually changed, so
 *   `useSyncExternalStore` never loops.
 *
 * No policy is duplicated here: drafts, tokens and serialization stay in the libs.
 */
import { useEffect, useRef, useSyncExternalStore } from "react";

import { ConnectionError } from "../lib/connectionError";
import type { SubagentRow } from "../lib/subagents";
import { TraceActions, type TraceActionSnapshot } from "../lib/subagentTraceActions";
import {
  TraceSubscription,
  type TraceClient,
  type TraceConnection,
  type TraceEventParams,
  type TraceSubscriptionSnapshot,
  type TraceTarget,
  traceTargetKey,
} from "../lib/subagentTraceSubscription";

export interface SubagentTraceView {
  subscription: TraceSubscriptionSnapshot;
  actions: TraceActionSnapshot | null;
  selected: TraceTarget | null;
}

export interface SubagentTraceController {
  subscribe(listener: () => void): () => void;
  getSnapshot(): SubagentTraceView;
  setConnection(connection: TraceConnection): void;
  setConnectionReady(connection: TraceConnection, ready: boolean): void;
  connectionFor(client: TraceClient, slot?: string): TraceConnection | null;
  open(target: TraceTarget): void;
  retry(): void;
  close(): void;
  tabChanged(visible: boolean): void;
  accept(connection: TraceConnection, params: TraceEventParams): void;
  disconnected(connection: TraceConnection): void;
  updateTask(target: TraceTarget, row: SubagentRow | null): void;
  setDraft(target: TraceTarget, text: string): void;
  send(target: TraceTarget): Promise<void>;
  preview(target: TraceTarget): Promise<void>;
  confirm(target: TraceTarget): Promise<void>;
  dismiss(target: TraceTarget): void;
  dropThread(slot: string, bindingGeneration: number, threadId: string): void;
  dropSlot(slot: string): void;
  resetBindings(): void;
  dispose(): void;
}

const NO_CONNECTION = "not connected";

function sameConnection(a: TraceConnection, b: TraceConnection): boolean {
  return a.client === b.client
    && a.slot === b.slot
    && a.serverBindingGeneration === b.serverBindingGeneration
    && a.connectionGeneration === b.connectionGeneration;
}

interface Source {
  connection: TraceConnection;
  ready: boolean;
}

export function createSubagentTraceController(): SubagentTraceController {
  const subscription = new TraceSubscription();
  const actions = new TraceActions();
  const listeners = new Set<() => void>();

  /** The current source per slot. Registration only; readiness is separate. */
  const sources = new Map<string, Source>();
  /** The source each client object came from, for events with no slot to trust. */
  const byClient = new Map<TraceClient, TraceConnection>();

  let selected: TraceTarget | null = null;
  let view: SubagentTraceView = {
    subscription: subscription.getSnapshot(),
    actions: null,
    selected: null,
  };
  let disposed = false;

  function emit(): void {
    if (disposed) return;
    const next: SubagentTraceView = {
      subscription: subscription.getSnapshot(),
      actions: selected === null ? null : actions.read(selected),
      selected,
    };
    if (
      next.subscription === view.subscription
      && next.actions === view.actions
      && next.selected === view.selected
    ) {
      return;
    }
    view = next;
    for (const listener of [...listeners]) listener();
  }

  // Rebuild the view whenever either machine changes.
  subscription.subscribe(emit);
  actions.subscribe(emit);

  function currentSource(slot: string): Source | null {
    return sources.get(slot) ?? null;
  }

  /**
   * The connection an action may use for this target: the slot's current source,
   * still ready, and naming the same server binding as the target.
   */
  function actionConnection(target: TraceTarget): TraceConnection | null {
    const source = currentSource(target.slot);
    if (source === null || !source.ready) return null;
    if (source.connection.serverBindingGeneration !== target.serverBindingGeneration) return null;
    return source.connection;
  }

  /** Drop whatever the given connection owned, keeping drafts and history. */
  function retire(connection: TraceConnection): void {
    subscription.disconnected(connection);
    actions.disconnected(connection);
  }

  const controller: SubagentTraceController = {
    subscribe(listener) {
      if (disposed) return () => {};
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },

    getSnapshot() {
      return view;
    },

    setConnection(connection) {
      if (disposed) return;
      const previous = currentSource(connection.slot);
      if (previous !== null && previous.connection.client !== connection.client) {
        // A new client replaces the old one: everything the old client owned
        // (owner, in-flight watch, preview) dies here. Selection, history and the
        // draft stay, because the user did not move.
        retire(previous.connection);
        byClient.delete(previous.connection.client);
      }
      sources.set(connection.slot, { connection, ready: false });
      byClient.set(connection.client, connection);
      // Retire a rebinding for a slot that moved to a new server binding.
      if (
        previous !== null
        && previous.connection.serverBindingGeneration !== connection.serverBindingGeneration
      ) {
        retire(previous.connection);
      }
      emit();
    },

    setConnectionReady(connection, ready) {
      if (disposed) return;
      const source = currentSource(connection.slot);
      // Only the current object and generation may change readiness. A stale
      // callback from a client that was already replaced must not re-open actions.
      if (source === null || !sameConnection(source.connection, connection)) return;

      source.ready = ready;
      if (!ready) {
        retire(connection);
        emit();
        return;
      }

      // Becoming ready opens manual retry; it never resubscribes by itself.
      emit();
    },

    connectionFor(client, slot) {
      const connection = byClient.get(client);
      if (connection === undefined) return null;
      if (slot !== undefined && connection.slot !== slot) return null;
      return connection;
    },

    open(target) {
      if (disposed) return;
      selected = { ...target };
      actions.setVisible(selected);
      const connection = actionConnection(selected);
      // A not-ready or missing source still records the selection, so the panel
      // can show the task and say why it cannot load yet.
      subscription.open(selected, connection);
      emit();
    },

    retry() {
      if (disposed || selected === null) return;
      subscription.retry(actionConnection(selected));
      emit();
    },

    close() {
      if (disposed) return;
      subscription.close();
      actions.setVisible(null);
      selected = null;
      emit();
    },

    tabChanged(visible) {
      if (disposed) return;
      if (!visible) {
        // Leaving the tab kills the confirmation, not the watch or the draft.
        actions.setVisible(null);
      } else if (selected !== null) {
        actions.setVisible(selected);
      }
      emit();
    },

    accept(connection, params) {
      if (disposed) return;
      // The subscription re-checks the full identity; this only rejects a source
      // we never registered, so an unrelated client cannot inject rows.
      if (byClient.get(connection.client) === undefined) return;
      subscription.accept(connection, params);
      emit();
    },

    disconnected(connection) {
      if (disposed) return;
      const known = byClient.get(connection.client);
      if (known === undefined) return;
      if (known.connectionGeneration !== connection.connectionGeneration) return;
      if (known.slot !== connection.slot) return;
      if (known.serverBindingGeneration !== connection.serverBindingGeneration) return;

      const source = currentSource(known.slot);
      if (source !== null && sameConnection(source.connection, known)) source.ready = false;
      retire(known);
      emit();
    },

    updateTask(target, row) {
      if (disposed) return;
      actions.updateTask(target, row);
      emit();
    },

    setDraft(target, text) {
      if (disposed) return;
      actions.setDraft(target, text);
      emit();
    },

    async send(target) {
      const connection = await requireAction(target);
      return actions.send(target, connection).finally(emit);
    },

    async preview(target) {
      const connection = await requireAction(target);
      return actions.preview(target, connection).finally(emit);
    },

    async confirm(target) {
      const connection = await requireAction(target);
      return actions.confirm(target, connection).finally(emit);
    },

    dismiss(target) {
      if (disposed) return;
      actions.dismiss(target);
      emit();
    },

    dropThread(slot, bindingGeneration, threadId) {
      if (disposed) return;
      actions.dropThread(slot, bindingGeneration, threadId);
      // A selection in that thread has nothing left to show.
      if (
        selected !== null
        && selected.slot === slot
        && selected.serverBindingGeneration === bindingGeneration
        && selected.threadId === threadId
      ) {
        controller.close();
        return;
      }
      emit();
    },

    dropSlot(slot) {
      if (disposed) return;
      actions.dropSlot(slot);
      if (selected !== null && selected.slot === slot) {
        controller.close();
        return;
      }
      emit();
    },

    resetBindings() {
      if (disposed) return;
      controller.close();
      actions.clear();
      sources.clear();
      byClient.clear();
      emit();
    },

    dispose() {
      if (disposed) return;
      controller.close();
      disposed = true;
      subscription.dispose();
      actions.clear();
      listeners.clear();
      sources.clear();
      byClient.clear();
    },
  };

  /**
   * Resolve the connection an action is allowed to use, or reject it.
   *
   * An unknown or mismatched target is not "the current selection": acting on it
   * would write to a view the user is not looking at, so it is refused loudly.
   * Routing the refusal through `TraceActions` is deliberate — the record gets the
   * connection error at the same place a missing connection would put it.
   */
  async function requireAction(target: TraceTarget): Promise<TraceConnection> {
    const visibleKey = selected === null ? null : traceTargetKey(selected);
    const connection = visibleKey === null || visibleKey !== traceTargetKey(target)
      ? null
      : actionConnection(target);
    if (connection === null) {
      // Sets the record's error and rejects with ConnectionError.
      await actions.send(target, null);
      throw new ConnectionError("disconnected", NO_CONNECTION);
    }
    return connection;
  }

  return controller;
}

/**
 * The hook: a stable controller per mount, plus its view.
 *
 * StrictMode runs setup → cleanup → setup synchronously. The cleanup must not
 * destroy the controller the second setup is about to reuse, so it closes the
 * open selection immediately (that is user-visible state) and only disposes if no
 * setup happened by the time the microtask queue drains.
 */
export function useSubagentTrace(): {
  controller: SubagentTraceController;
  view: SubagentTraceView;
} {
  // One controller per mount, created lazily so a render never issues a watch.
  const ref = useRef<SubagentTraceController | null>(null);
  if (ref.current === null) ref.current = createSubagentTraceController();
  const controller = ref.current;

  const view = useSyncExternalStore(
    controller.subscribe,
    controller.getSnapshot,
    controller.getSnapshot,
  );

  // StrictMode mounts, cleans up and mounts again in one tick. The cleanup closes
  // the selection (user-visible, must not survive) and only disposes when no later
  // setup claimed the controller by the time the microtask queue drains.
  const generation = useRef(0);

  useEffect(() => {
    generation.current += 1;
    const mine = generation.current;
    return () => {
      controller.close();
      queueMicrotask(() => {
        if (generation.current === mine) controller.dispose();
      });
    };
  }, [controller]);

  return { controller, view };
}
