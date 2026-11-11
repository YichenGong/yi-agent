import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { isRemoteClient } from "../lib/platform";
import {
  isHeldLease,
  leaseDecision,
  wantedProjects,
  type AcquireView,
  type LeaseDecision,
} from "../lib/runtimeLease";
import {
  ACQUIRE_FAILED_REASON,
  acceptsStartupReply,
  type RuntimeStartupIssue,
} from "../lib/runtimeStartupIssue";

/**
 * Keeps the desktop shell's runtime leases aligned with what the user is doing.
 *
 * The decision itself lives in `lib/runtimeLease.ts` and is pure, so the policy is
 * tested without a browser. This hook is only the wiring:
 *
 * * it runs the decision whenever the open thread or a busy thread changes,
 * * it serialises the resulting calls (two rapid switches must not interleave an
 *   acquire and a release of the same project),
 * * it reports a preparation failure against its project, so the user can see it
 *   and retry deliberately, and
 * * it swallows nothing that matters: a failure does not break the conversation,
 *   but it is no longer invisible either.
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

export interface RuntimeLeaseFeedback {
  /** Preparation problems, keyed by project path. */
  issues: ReadonlyMap<string, RuntimeStartupIssue>;
  /**
   * Asks the shell to prepare `project` again.
   *
   * Deliberately explicit: a refusal is not retried in a loop, because a blocked
   * runtime may be waiting for a decision the user has not made yet. This only
   * re-runs the acquire command; it never sends a turn or replays a message.
   */
  retryProject(project: string): void;
}

export function useRuntimeLease(deps: LeaseDeps): RuntimeLeaseFeedback {
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
  // Per-project request sequence. A late answer for an older request is dropped,
  // so it cannot overwrite a newer failure or a newer success.
  const requestSeq = useRef<Map<string, number>>(new Map());
  // Projects an explicit retry has asked to acquire again, even though `owned`
  // still lists them (a held-but-blocked project keeps its lease).
  const forced = useRef<Set<string>>(new Set());
  const [retryTick, setRetryTick] = useState(0);
  const [issues, setIssues] = useState<ReadonlyMap<string, RuntimeStartupIssue>>(
    () => new Map(),
  );

  const retryProject = useCallback((project: string) => {
    forced.current.add(project);
    setRetryTick((tick) => tick + 1);
  }, []);

  useEffect(() => {
    if (remote.current) return;
    const wantedNow = new Set(
      wantedProjects({
        activeProject: deps.activeProject,
        threads: deps.threads,
        pluginProjects: deps.pluginProjects,
      }),
    );
    // A held-but-blocked project must be acquirable again when the user asks,
    // even though it is still in `owned`. Every other owned project stays in the
    // decision so leaving it still releases its lease.
    const ownedForDecision = [...owned.current].filter(
      (project) => !(forced.current.has(project) && wantedNow.has(project)),
    );
    const decision: LeaseDecision = leaseDecision({
      activeProject: deps.activeProject,
      threads: deps.threads,
      pluginProjects: deps.pluginProjects,
      owned: ownedForDecision,
    });
    wanted.current = wantedNow;
    const retried = new Set(forced.current);
    forced.current.clear();

    if (decision.acquire.length === 0 && decision.release.length === 0) return;

    // Reserve the *intent*, not a lease: a second run before the first finishes
    // must not decide to release a project whose acquire is still in flight. The
    // reservation is released again if the answer is not a held lease.
    for (const project of decision.acquire) owned.current.add(project);
    for (const project of decision.release) owned.current.delete(project);

    const noteIssue = (project: string, issue: RuntimeStartupIssue) => {
      setIssues((previous) => {
        const next = new Map(previous);
        const current = requestSeq.current.get(project) ?? 0;
        // Only the newest request may speak for its project.
        if (!acceptsStartupReply(current, issue.requestSeq)) return previous;
        next.set(project, issue);
        return next;
      });
    };
    const clearIssue = (project: string) => {
      setIssues((previous) => {
        if (!previous.has(project)) return previous;
        const next = new Map(previous);
        next.delete(project);
        return next;
      });
    };

    chain.current = chain.current.then(async () => {
      for (const project of decision.acquire) {
        const seq = (requestSeq.current.get(project) ?? 0) + 1;
        requestSeq.current.set(project, seq);
        let view: AcquireView;
        try {
          view = (await invoke("runtime_acquire_project", { project })) as AcquireView;
        } catch {
          // A thrown command is a refusal too, but the exception text can carry
          // paths or credential-shaped values, so the page gets a fixed sentence.
          owned.current.delete(project);
          if (wanted.current.has(project)) {
            noteIssue(project, {
              project,
              requestSeq: seq,
              stage: "acquire",
              reason: retried.has(project)
                ? `${ACQUIRE_FAILED_REASON}（已重试仍失败）`
                : ACQUIRE_FAILED_REASON,
            });
          }
          continue;
        }

        // Asked for again while this answer was in flight? Then it belongs to an
        // older request and must not decide anything.
        if (!acceptsStartupReply(requestSeq.current.get(project) ?? 0, seq)) {
          continue;
        }

        const held = isHeldLease(view);
        const ready = view.state === "managedReady";
        if (!held) owned.current.delete(project);

        // A late answer for a project the decision has since dropped must not
        // resurrect it: release a lease it did bring, and keep no failure for a
        // project nobody is looking at.
        if (!wanted.current.has(project)) {
          owned.current.delete(project);
          const lateScope = scopes.current.get(project) ?? view.scopeKey ?? null;
          if (held && lateScope) {
            await invoke("runtime_release_project", { scopeKey: lateScope }).catch(
              () => {},
            );
            scopes.current.delete(project);
          }
          continue;
        }

        if (!ready) {
          noteIssue(project, {
            project,
            requestSeq: seq,
            attemptId: view.attemptId,
            scopeKey: view.scopeKey || undefined,
            stage: "acquire",
            state: view.state,
            reason: view.reason ?? view.state,
          });
        } else {
          clearIssue(project);
        }

        if (!held) continue;

        // Recorded only after the shell reported a held lease: the shell holds
        // the resolved scope from this point on, so it can answer status reads
        // for a runtime that refused a lease (embedded or legacy) as well.
        // Always ask the shell: it owns the one canonicalisation of a project
        // path, and a release must speak the key it resolved, not one echoed in
        // an acquire reply.
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
  }, [deps.activeProject, deps.revision, retryTick]);

  return { issues, retryProject };
}
