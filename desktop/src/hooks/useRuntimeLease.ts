import { useEffect, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import { isRemoteClient } from "../lib/platform";
import { leaseDecision, type LeaseDecision } from "../lib/runtimeLease";

/**
 * Keeps the desktop shell's runtime leases aligned with what the user is doing.
 *
 * The decision itself lives in `lib/runtimeLease.ts` and is pure, so the policy is
 * tested without a browser. This hook is only the wiring:
 *
 * * it runs the decision whenever the open thread or a busy thread changes,
 * * it serialises the resulting calls (two rapid switches must not interleave an
 *   acquire and a release of the same project),
 * * and it swallows errors: a lease failure must not break the conversation, it
 *   just leaves the runtime attached longer than ideal.
 *
 * It is a no-op for the remote web client, which has no Tauri shell and no
 * sidecar of its own.
 */
export interface LeaseDeps {
  /** The project of the open thread, or null. */
  activeProject: string | null;
  /** Every known thread's project and status. */
  threads: ReadonlyArray<{ cwd: string; status?: string }>;
  /** A value that changes whenever the wanted set may have changed. */
  revision: string;
}

export function useRuntimeLease(deps: LeaseDeps) {
  // What this shell believes it holds. Kept in a ref, not state: it is not
  // rendered, and making it state would re-render the whole app on every switch.
  const owned = useRef<Set<string>>(new Set());
  // The scope key the shell resolved for each project. Release needs the key, not
  // the path: the key is the canonical runtime directory, and the shell refuses to
  // act on a key it never resolved. Asking the shell keeps one canonicalisation
  // instead of two that could disagree about which directory a scope names.
  const scopes = useRef<Map<string, string>>(new Map());
  // Serialise the calls: each run waits for the previous one.
  const chain = useRef<Promise<void>>(Promise.resolve());
  const remote = useRef(isRemoteClient());

  useEffect(() => {
    if (remote.current) return;
    const decision: LeaseDecision = leaseDecision({
      activeProject: deps.activeProject,
      threads: deps.threads,
      owned: [...owned.current],
    });
    if (decision.acquire.length === 0 && decision.release.length === 0) return;

    // Optimistically reserve: a second run before the first finishes must not
    // decide to release a project whose acquire is still in flight.
    for (const project of decision.acquire) owned.current.add(project);
    for (const project of decision.release) owned.current.delete(project);

    chain.current = chain.current.then(async () => {
      for (const project of decision.acquire) {
        try {
          await invoke("runtime_acquire_project", { project });
          // Recorded only after the shell accepted it: the shell holds the resolved
          // scope from this point on, so it can answer status reads for a runtime
          // that refused a lease (embedded or legacy) as well.
          const scopeKey = await invoke<string | null>("runtime_scope_for_project", {
            project,
          });
          if (scopeKey) scopes.current.set(project, scopeKey);
        } catch {
          // Not fatal: the runtime simply stays unleased, which is the state the
          // app was in before this feature existed.
          owned.current.delete(project);
        }
      }
      for (const project of decision.release) {
        const scopeKey = scopes.current.get(project);
        // Without a resolved key the shell has nothing to release; inventing one
        // from the path would ask it to act on a scope it never resolved.
        if (!scopeKey) continue;
        try {
          await invoke("runtime_release_project", { scopeKey });
          scopes.current.delete(project);
        } catch {
          // Releasing is best-effort here; the quit drain is the authoritative
          // path and reports its own failures.
        }
      }
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [deps.activeProject, deps.revision]);
}
