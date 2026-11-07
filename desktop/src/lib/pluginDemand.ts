/**
 * Which UI consumers want a plugin service, and what to tell the shell.
 *
 * A plugin service is shared: the guardian stops it when its **last** holder
 * releases. That makes per-view acquire/release the wrong shape -- one board view
 * closing would stop a service another view is still using, and two views of the
 * same board would look like two clients to the runtime.
 *
 * So the page names *consumers*, and this module folds them into one intent per
 * `(scope, plugin)`. Only the first consumer of a plugin produces an acquire; only
 * the last one leaving produces a release. Everything in between is a local fact
 * that must not cross the process boundary.
 *
 * Demand is deliberately not a lease: it cannot renew one, cannot authorise work,
 * and cannot keep a runtime alive by itself. It only answers "may this shared
 * plugin be stopped yet".
 */

/** One on-screen consumer of a plugin, e.g. one board view. */
export interface PluginConsumer {
  /** The project the plugin serves (the path the shell resolved a scope for). */
  project: string;
  /** The plugin id, as the manifest names it. */
  plugin: string;
  /** Identifies this view, so it can withdraw exactly its own demand. */
  consumer: string;
}

/** What the shell must be told, per `(scope, plugin)`. */
export interface PluginDemandIntent {
  project: string;
  plugin: string;
  consumers: string[];
}

function key(project: string, plugin: string): string {
  // `\u0000` cannot appear in a path or a plugin id, so two different pairs can
  // never collide into one key.
  return `${project}\u0000${plugin}`;
}

/**
 * Folds consumers into one intent per `(project, plugin)`.
 *
 * Sorted so a log of the resulting calls is readable and two runs with the same
 * input produce the same order.
 */
export function aggregateDemand(consumers: ReadonlyArray<PluginConsumer>): PluginDemandIntent[] {
  const grouped = new Map<string, PluginDemandIntent>();
  for (const consumer of consumers) {
    const grouped_key = key(consumer.project, consumer.plugin);
    const existing = grouped.get(grouped_key);
    if (existing) {
      if (!existing.consumers.includes(consumer.consumer)) {
        existing.consumers.push(consumer.consumer);
      }
    } else {
      grouped.set(grouped_key, {
        project: consumer.project,
        plugin: consumer.plugin,
        consumers: [consumer.consumer],
      });
    }
  }
  return [...grouped.values()]
    .map((intent) => ({ ...intent, consumers: [...intent.consumers].sort() }))
    .sort((a, b) => key(a.project, a.plugin).localeCompare(key(b.project, b.plugin)));
}

function consumerKey(consumer: PluginConsumer): string {
  // The consumer name already has to be unique per view; pairing it with the plugin
  // keeps the same view name in two plugins from colliding.
  return `${key(consumer.project, consumer.plugin)}\u0000${consumer.consumer}`;
}

/**
 * Which individual consumers to register and withdraw.
 *
 * The native side tracks consumers, not intents: it decides "the last one left" by
 * counting. So the page has to send *every* joining consumer, and only the exact
 * consumer that left -- sending an intent-level release would withdraw somebody
 * else's view.
 */
export function consumerDelta(
  wanted: ReadonlyArray<PluginConsumer>,
  held: ReadonlyArray<PluginConsumer>,
): { acquire: PluginConsumer[]; release: PluginConsumer[] } {
  const wantedKeys = new Set(wanted.map(consumerKey));
  const heldKeys = new Set(held.map(consumerKey));
  return {
    acquire: wanted.filter((consumer) => !heldKeys.has(consumerKey(consumer))),
    release: held.filter((consumer) => !wantedKeys.has(consumerKey(consumer))),
  };
}

/**
 * What to acquire and release so the shell's view matches the mounted consumers.
 *
 * `held` is what the shell was last told, not what the runtime says: the native
 * side aggregates too, and re-sending an acquire it already has is harmless while
 * a *missed* release leaves a service running for nobody.
 */
export function demandDelta(
  wanted: ReadonlyArray<PluginConsumer>,
  held: ReadonlyArray<PluginConsumer>,
): { acquire: PluginDemandIntent[]; release: PluginDemandIntent[] } {
  const delta = consumerDelta(wanted, held);
  return {
    acquire: aggregateDemand(delta.acquire),
    release: aggregateDemand(delta.release),
  };
}

/**
 * Which projects have a plugin view on screen.
 *
 * These are demand for *the project*, not just for the plugin: a board-only window
 * has no thread, so nothing else would ask for a runtime and the plugin would wait
 * forever for a service no lease keeps alive.
 */
export function projectsWithPluginViews(consumers: ReadonlyArray<PluginConsumer>): string[] {
  const projects = new Set(consumers.map((consumer) => consumer.project));
  return [...projects].sort();
}
