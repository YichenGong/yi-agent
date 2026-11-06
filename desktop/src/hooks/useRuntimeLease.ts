import { useEffect, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import { isRemoteClient } from "../lib/platform";
import {
  isHeldLease,
  leaseDecision,
  wantedProjects,
  type AcquireView,
  type LeaseDecision,
} from "../lib/runtimeLease";

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
  /**
   * Projects with a plugin view on screen.
   *
   * A board-only window has no thread, so this is the only reason its project is
   * wanted at all. Omitting it would make the plugin wait for a runtime nobody
   * kept alive.
   */
  pluginProjects?: ReadonlyArray<string>;
  /** A value that changes whenever the wanted set may have changed. */
  revision: string;
}

export function useRuntimeLease(deps: LeaseDeps) {
  // What this shell believes it holds. Kept in a ref, not state: it is not
  // rendered, and making it state would re-render the whole app on every switch.
  //
  // The single source of truth is still the native side (`runtime_registered_projects`);
  // this set only decides *when to ask*, so a stale entry costs one refused
  // release, never a wrong "this project is ready".
  const owned = useRef<Set<string>>(new Set());
  // The wanted set as of the latest decision. A preparation that finishes later
  // compares against this: a project the user has since left must not be recorded
  // as held, or it would never be released.
  const wanted = useRef<Set<string>>(new Set());
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
      pluginProjects: deps.pluginProjects,
      owned: [...owned.current],
    });
    wanted.current = new Set(
      wantedProjects({
        activeProject: deps.activeProject,
        threads: deps.threads,
        pluginProjects: deps.pluginProjects,
      }),
    );

    if (decision.acquire.length === 0 && decision.release.length === 0) return;

    // Reserve the *intent*, not a lease: a second run before the first finishes
    // must not decide to release a project whose acquire is still in flight. The
    // reservation is released again if the answer is not `managedReady`.
    for (const project of decision.acquire) owned.current.add(project);
    for (const project of decision.release) owned.current.delete(project);

    chain.current = chain.current.then(async () => {
      for (const project of decision.acquire) {
        try {
          const view = (await invoke("runtime_acquire_project", {
            project,
          })) as AcquireView;
          if (!isHeldLease(view)) {
            // The shell answered, but not with a lease. Dropping the reservation
            // is what makes the next decision retry: neither `starting` nor
            // `blocked` is something that may be released.
            owned.current.delete(project);
            continue;
          }
          // Recorded only after the shell reported a live lease: the shell holds
          // the resolved scope from this point on, so it can answer status reads
          // for a runtime that refused a lease (embedded or legacy) as well.
          const scopeKey = await invoke<string | null>("runtime_scope_for_project", {
            project,
          });
          if (scopeKey) scopes.current.set(project, scopeKey);
          // A late answer for a project the decision has since dropped must not
          // resurrect it: release it now instead of leaving a lease with no owner.
          if (!wanted.current.has(project)) {
            owned.current.delete(project);
            if (scopeKey) {
              await invoke("runtime_release_project", { scopeKey }).catch(() => {});
              scopes.current.delete(project);
            }
          }
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
