import { useEffect, useRef, useState, useSyncExternalStore } from "react";

import { isRemoteClient } from "../lib/platform";
import {
  consumerDelta,
  projectsWithPluginViews,
  type PluginConsumer,
} from "../lib/pluginDemand";
import { pluginConsumersSnapshot, subscribePluginConsumers } from "../lib/pluginDemandRegistry";

/**
 * Tells the shell which plugins on screen still need a service.
 *
 * Three rules decide the shape:
 *
 * 1. **Consumers, not plugins.** The native side stops a service when its last
 *    consumer leaves, so each mounted view registers and withdraws itself. A
 *    plugin-level acquire/release would stop a service another view still uses.
 * 2. **Demand is not a lease.** Nothing here grants authority or keeps a runtime
 *    alive on its own; it only answers "may this shared plugin be stopped".
 * 3. **The project counts, not just the plugin.** A board-only window has no
 *    thread, so its project has to enter the lease-wanted set or the plugin waits
 *    for a runtime nobody kept alive. That is what `projectsWithPluginViews` is for,
 *    and it is returned from here so the caller can put it in the lease decision.
 *
 * The shell resolves the project to a scope key: computing one here would be a
 * second canonicalisation that could name a runtime the shell never opened.
 */
export interface Writer {
  acquire(scopeKey: string, plugin: string, consumer: string): Promise<unknown>;
  release(scopeKey: string, plugin: string, consumer: string): Promise<unknown>;
  scopeFor(project: string): Promise<string | null>;
}

/** The desktop writer, over the shell's own commands. */
export function localDemandWriter(): Writer {
  return {
    async acquire(scopeKey, plugin, consumer) {
      const { invoke: call } = await import("@tauri-apps/api/core");
      return call("runtime_plugin_demand_acquire", { scopeKey, plugin, consumer });
    },
    async release(scopeKey, plugin, consumer) {
      const { invoke: call } = await import("@tauri-apps/api/core");
      return call("runtime_plugin_demand_release", { scopeKey, plugin, consumer });
    },
    async scopeFor(project) {
      const { invoke: call } = await import("@tauri-apps/api/core");
      return (await call<string | null>("runtime_scope_for_project", { project })) ?? null;
    },
  };
}

export interface PluginDemandDeps {
  /** Defaults to the shell's commands; tests inject a recorder. */
  writer?: Writer;
  /** How long to wait before retrying demand whose scope the shell has not resolved. */
  retryMs?: number;
}

/** How many times a block of demand may wait for a scope before giving up. */
const MAX_SCOPE_RETRIES = 20;

/**
 * Syncs the registry to the shell, and returns the projects with plugin views.
 *
 * The returned list is recomputed from the same snapshot, so a caller that feeds it
 * into `useRuntimeLease` sees the plugin projects and the demand in one render --
 * two independent subscriptions could disagree for a frame, which would either
 * acquire a scope nobody needs or drop one still in use.
 */
export function usePluginDemand(deps: PluginDemandDeps = {}): string[] {
  const consumers = useSyncExternalStore(
    subscribePluginConsumers,
    pluginConsumersSnapshot,
    pluginConsumersSnapshot,
  );
  const writer = useRef<Writer | null>(null);
  if (writer.current === null) {
    writer.current = deps.writer ?? (isRemoteClient() ? null : localDemandWriter());
  }
  const remote = writer.current === null;
  // What the shell has been told, per consumer. A ref because it is bookkeeping,
  // not rendered state.
  const held = useRef<PluginConsumer[]>([]);
  const scopes = useRef<Map<string, string>>(new Map());
  const chain = useRef<Promise<void>>(Promise.resolve());
  // Bumped when demand has to wait for the shell to resolve a scope. It is a state
  // counter rather than a bare timer so the effect (and only the effect) re-runs:
  // the retry is the *same* decision, not a new one.
  const [attempt, setAttempt] = useState(0);
  const retries = useRef(0);
  const retryMs = deps.retryMs ?? 250;
  const mounted = useRef(false);
  const retryTimers = useRef(new Set<ReturnType<typeof setTimeout>>());

  useEffect(() => {
    mounted.current = true;
    const timers = retryTimers.current;
    return () => {
      mounted.current = false;
      for (const timer of timers) clearTimeout(timer);
      timers.clear();
    };
  }, []);

  useEffect(() => {
    if (remote) return;
    const active = writer.current;
    if (!active) return;
    const delta = consumerDelta(consumers, held.current);
    if (delta.acquire.length === 0 && delta.release.length === 0) return;
    // Record the intent before the await, so a second decision while these calls
    // are in flight does not send them again.
    held.current = consumers;

    chain.current = chain.current.then(async () => {
      let waitingForScope = false;
      for (const consumer of delta.acquire) {
        try {
          const scopeKey = await scopeFor(active, consumer.project, scopes.current);
          if (!scopeKey) {
            // The shell has not resolved this project, which means no lease has
            // asked for it yet -- the lease hook acquires it in the same render.
            // Forget the intent so the retry below is a real attempt, and wait.
            waitingForScope = true;
            held.current = held.current.filter(
              (existing) =>
                !(
                  existing.project === consumer.project &&
                  existing.plugin === consumer.plugin &&
                  existing.consumer === consumer.consumer
                ),
            );
            continue;
          }
          await active.acquire(scopeKey, consumer.plugin, consumer.consumer);
        } catch {
          // Best-effort: a failed demand leaves the plugin unstarted, which is the
          // state it was in before this feature existed.
        }
      }
      if (mounted.current && waitingForScope && retries.current < MAX_SCOPE_RETRIES) {
        retries.current += 1;
        const timer = setTimeout(() => {
          retryTimers.current.delete(timer);
          if (mounted.current) setAttempt((value) => value + 1);
        }, retryMs);
        retryTimers.current.add(timer);
      } else if (!waitingForScope) {
        retries.current = 0;
      }
      for (const consumer of delta.release) {
        const scopeKey = scopes.current.get(consumer.project);
        if (!scopeKey) continue;
        try {
          await active.release(scopeKey, consumer.plugin, consumer.consumer);
        } catch {
          // Same: release is retried by the next decision if the view returns.
        }
      }
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [consumers, remote, attempt]);

  return projectsWithPluginViews(consumers);
}

async function scopeFor(
  writer: Writer,
  project: string,
  cache: Map<string, string>,
): Promise<string | null> {
  const known = cache.get(project);
  if (known) return known;
  const scopeKey = await writer.scopeFor(project);
  if (scopeKey) cache.set(project, scopeKey);
  return scopeKey;
}
