/**
 * The set of plugin views currently on screen, as one subscribable list.
 *
 * A plugin view can be mounted anywhere (a settings section, a board, an embedded
 * panel), and the shell needs one aggregated answer, not one per component. So
 * views register themselves here and exactly one subscriber -- the App -- decides
 * what to tell the native side.
 *
 * Registration is by *consumer name*, not by plugin: the native side counts
 * consumers and stops a service only when the last one leaves, so a view must be
 * able to withdraw precisely its own demand. Two views of the same board are two
 * consumers of one service; one closing must not stop it for the other.
 *
 * Kept outside React state on purpose: mounting a view must not re-render the
 * whole app, and the list is read through `useSyncExternalStore`.
 */

import type { PluginConsumer } from "./pluginDemand";

let consumers: PluginConsumer[] = [];
const listeners = new Set<() => void>();

function emit() {
  for (const listener of [...listeners]) listener();
}

function same(a: PluginConsumer, b: PluginConsumer): boolean {
  return a.project === b.project && a.plugin === b.plugin && a.consumer === b.consumer;
}

/**
 * Adds one consumer. Returns the unsubscribe function, which removes that consumer
 * and nobody else's.
 *
 * Registering the same consumer twice is idempotent: a re-mount that fails to
 * unsubscribe first must not make the count wrong, because the count is what
 * decides when the service stops.
 */
export function registerPluginConsumer(consumer: PluginConsumer): () => void {
  if (!consumers.some((existing) => same(existing, consumer))) {
    consumers = [...consumers, consumer];
    emit();
  }
  let released = false;
  return () => {
    if (released) return;
    released = true;
    const next = consumers.filter((existing) => !same(existing, consumer));
    if (next.length === consumers.length) return;
    consumers = next;
    emit();
  };
}

export function pluginConsumersSnapshot(): PluginConsumer[] {
  return consumers;
}

export function subscribePluginConsumers(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** Only for tests: drops every registration without touching the mounted views. */
export function resetPluginConsumers(): void {
  if (consumers.length === 0) return;
  consumers = [];
  emit();
}
