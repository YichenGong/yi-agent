/**
 * Binds the lifecycle state machine to React.
 *
 * The hook owns exactly two things the machine cannot: the effect lifetime, and
 * the "which scope is on screen" argument. Everything else -- polling cadence,
 * phase transitions, recovery by operation id -- lives in
 * `lib/runtimeLifecycle`, where it is testable without a DOM.
 *
 * A scope switch tears the machine down and builds a new one. Reusing it would
 * carry the previous project's preview token into the next project's
 * confirmation, which is precisely the mistake the token exists to prevent.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  createRuntimeLifecycle,
  type Deps,
  type LifecycleState,
  type RuntimeControl,
  type RuntimeLifecycle,
} from "../lib/runtimeLifecycle";

export interface UseRuntimeLifecycleOptions {
  /** The scope the shell resolved, or null when no project is open. */
  scopeKey: string | null;
  /** Null disables the hook entirely (no project, or a build without control). */
  control: RuntimeControl | null;
  /**
   * Reads the operation id the shell is tracking for this scope.
   *
   * The id is not persisted in the WebView: it is stored where the confirmation
   * was issued (the shell), so a reloaded WebView resumes the same operation and
   * an id from a previous run cannot be mistaken for the current one.
   */
  readOperationId?: (scopeKey: string) => Promise<string | null>;
}

/**
 * Resolves the scope key the shell uses for `project`.
 *
 * The key is asked for, never computed: it is a canonical runtime directory, and a
 * WebView that derived its own would name a scope the shell never opened (a
 * symlinked project is enough to disagree). `null` means "ask again later" -- no
 * project is open, the shell has not resolved it yet, or this build has no local
 * runtime at all.
 *
 * A failed resolution is not an error to show: the version feature is additive, so
 * the app must behave exactly as it did before when the shell cannot answer.
 */
export function useRuntimeScopeKey(project: string | null, enabled: boolean): string | null {
  const [scopeKey, setScopeKey] = useState<string | null>(null);
  useEffect(() => {
    if (project === null || !enabled) {
      setScopeKey(null);
      return;
    }
    let cancelled = false;
    void (async () => {
      try {
        const { invoke } = await import("@tauri-apps/api/core");
        const resolved = await invoke<string | null>("runtime_scope_for_project", {
          project,
        });
        if (!cancelled) setScopeKey(resolved ?? null);
      } catch {
        if (!cancelled) setScopeKey(null);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [project, enabled]);
  return scopeKey;
}

const IDLE: LifecycleState = {
  status: null,
  preview: null,
  operation: null,
  phase: "idle",
  error: null,
  operationId: null,
  busy: false,
  controlUnavailable: false,
};

export function useRuntimeLifecycle(options: UseRuntimeLifecycleOptions) {
  const { scopeKey, control, readOperationId } = options;
  const [state, setState] = useState<LifecycleState>(IDLE);
  const lifecycleRef = useRef<RuntimeLifecycle | null>(null);

  // Kept in refs so a callback identity change does not rebuild the machine (and
  // with it, throw away a preview the user is looking at).
  const readRef = useRef(readOperationId);
  readRef.current = readOperationId;

  useEffect(() => {
    if (scopeKey === null || control === null) {
      setState(IDLE);
      return;
    }
    const deps: Deps = {
      control,
      rememberedOperationId: (scope) => readRef.current?.(scope) ?? Promise.resolve(null),
    };
    const lifecycle = createRuntimeLifecycle(scopeKey, deps);
    lifecycleRef.current = lifecycle;
    setState(lifecycle.getState());
    const unsubscribe = lifecycle.subscribe(setState);
    lifecycle.start();
    void lifecycle.resume();
    return () => {
      unsubscribe();
      lifecycle.dispose();
      lifecycleRef.current = null;
    };
  }, [scopeKey, control]);

  const prepare = useCallback(() => lifecycleRef.current?.prepare(), []);
  const confirm = useCallback(() => lifecycleRef.current?.confirm(), []);
  const cancel = useCallback(() => lifecycleRef.current?.cancel(), []);
  const refresh = useCallback(() => lifecycleRef.current?.refresh(), []);

  const enabled = scopeKey !== null && control !== null;
  return useMemo(
    () => ({ state, enabled, prepare, confirm, cancel, refresh }),
    [state, enabled, prepare, confirm, cancel, refresh],
  );
}
