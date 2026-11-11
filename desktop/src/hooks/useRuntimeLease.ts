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
  isStartupProblem,
  startupIssueReason,
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
 * * it can prepare one project on demand, which is what keeps a first
 *   `thread/start` from creating a conversation with no tools.
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
  /**
   * Prepares one project and answers what the shell said.
   *
   * The caller is the "create a conversation" path: it must know whether the
   * runtime is `managedReady` *before* `thread/start`, or the conversation is
   * created against a project that can never run a tool. Returns `null` when this
   * client cannot prepare locally (remote) or the shell threw; the issue map
   * explains a refusal.
   *
   * It records everything the decision needs, so the project is not acquired a
   * second time when it becomes the active one.
   */
  prepareProject(project: string): Promise<AcquireView | null>;
}

export function useRuntimeLease(deps: LeaseDeps): RuntimeLeaseFeedback {
  // What this shell believes it holds. Kept in a ref, not state: it is not
  // rendered, and making it state would re-render the whole app on every switch.
  //
  // The single source of truth is still the native side (`runtime_registered_projects`);
  // this set only decides *when to ask*, so a stale entry costs one refused
  // release, never a wrong "this project is ready".
  const owned = useRef<Set<string>>(new Set());
  // Projects whose acquire actually reported a held lease. A project whose
  // acquire is still in flight is in `owned` as a reservation only: releasing it
  // now would act on a lease that does not exist yet, and the late answer is what
  // releases it if the user has since left.
  const held = useRef<Set<string>>(new Set());
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

  // Only fixed wording reaches the page. A native reason is diagnostic and can
  // carry paths or error text, so it is never rendered.
  const noteIssue = useCallback((project: string, issue: RuntimeStartupIssue) => {
    // A reply without a state is a failed command, not a healthy runtime; the
    // fixed table has no entry for it, so it falls back to the generic sentence.
    const reason =
      startupIssueReason(issue.state) ??
      (issue.state === undefined
        ? ACQUIRE_FAILED_REASON
        : issue.reason || ACQUIRE_FAILED_REASON);
    setIssues((previous) => {
      const current = requestSeq.current.get(project) ?? 0;
      // Only the newest request may speak for its project.
      if (!acceptsStartupReply(current, issue.requestSeq)) return previous;
      const next = new Map(previous);
      if (!isStartupProblem(issue.state)) {
        next.delete(project);
        return next;
      }
      next.set(project, { ...issue, reason });
      return next;
    });
  }, []);

  const clearIssue = useCallback((project: string) => {
    setIssues((previous) => {
      if (!previous.has(project)) return previous;
      const next = new Map(previous);
      next.delete(project);
      return next;
    });
  }, []);

  /**
   * Applies one acquire answer: what is held, what the page is told, and what a
   * project the user has since left still owes back.
   */
  const settleAcquire = useCallback(
    async (project: string, seq: number, view: AcquireView) => {
      // Asked for again while this answer was in flight? Then it belongs to an
      // older request and must not decide anything.
      if (!acceptsStartupReply(requestSeq.current.get(project) ?? 0, seq)) {
        return;
      }

      const leasesHeld = isHeldLease(view);
      const ready = view.state === "managedReady";
      if (!leasesHeld) {
        owned.current.delete(project);
        held.current.delete(project);
      } else {
        held.current.add(project);
      }

      // A late answer for a project the decision has since dropped must not
      // resurrect it: release a lease it did bring, and keep no failure for a
      // project nobody is looking at.
      if (!wanted.current.has(project)) {
        owned.current.delete(project);
        held.current.delete(project);
        const lateScope = scopes.current.get(project) ?? view.scopeKey ?? null;
        if (leasesHeld && lateScope) {
          await invoke("runtime_release_project", { scopeKey: lateScope }).catch(
            () => {},
          );
          scopes.current.delete(project);
        }
        return;
      }

      if (!ready) {
        noteIssue(project, {
          project,
          requestSeq: seq,
          attemptId: view.attemptId,
          scopeKey: view.scopeKey || undefined,
          stage: "acquire",
          state: view.state,
          // Placeholder only: `noteIssue` replaces it from the fixed table.
          reason: "",
        });
      } else {
        clearIssue(project);
      }

      if (!leasesHeld) return;

      // Recorded only after the shell reported a held lease: the shell holds the
      // resolved scope from this point on, so it can answer status reads for a
      // runtime that refused a lease (embedded or legacy) as well.
      //
      // Always ask the shell: it owns the one canonicalisation of a project path,
      // and a release must speak the key it resolved, not one echoed in an
      // acquire reply. A failed lookup is *not* fatal: the chain is shared by
      // every later acquire and release, so letting the rejection through would
      // stop them all and strand the lease. The key is asked for again when a
      // release is actually needed.
      const scopeKey = await invoke<string | null>("runtime_scope_for_project", {
        project,
      }).catch((): string | null => null);
      if (scopeKey) scopes.current.set(project, scopeKey);
      // A late answer for a project the decision has since dropped must not
      // resurrect it: release it now instead of leaving a lease with no owner.
      if (!wanted.current.has(project)) {
        owned.current.delete(project);
        held.current.delete(project);
        if (scopeKey) {
          await invoke("runtime_release_project", { scopeKey }).catch(() => {});
          scopes.current.delete(project);
        }
      }
    },
    [clearIssue, noteIssue],
  );

  const acquireOnce = useCallback(
    async (project: string, seq: number): Promise<AcquireView | null> => {
      let view: AcquireView | null = null;
      try {
        const answer = await invoke("runtime_acquire_project", { project });
        // A shell that has no such command (or one that answers nothing) is not
        // evidence of a lease. Treat it as a refusal rather than dereferencing
        // whatever came back.
        view =
          answer && typeof (answer as AcquireView).state === "string"
            ? (answer as AcquireView)
            : null;
      } catch {
        view = null;
      }
      if (view === null) {
        // A thrown command is a refusal too, but the exception text can carry
        // paths or credential-shaped values, so the page gets a fixed sentence.
        // A failed command gives no evidence that a previous lease disappeared.
        // Preserve its release obligation; only discard a never-held reservation.
        if (!held.current.has(project)) owned.current.delete(project);
        if (wanted.current.has(project)) {
          noteIssue(project, {
            project,
            requestSeq: seq,
            stage: "acquire",
            reason: ACQUIRE_FAILED_REASON,
          });
        }
        return null;
      }
      await settleAcquire(project, seq, view);
      return view;
    },
    [noteIssue, settleAcquire],
  );

  /** Queues work on the one chain, so no two decisions interleave. */
  const enqueue = useCallback(<T,>(work: () => Promise<T>): Promise<T> => {
    const run = chain.current.then(work);
    // The chain is the only path every later decision takes, so one unexpected
    // rejection must not end it for the life of the app.
    chain.current = run.then(
      () => {},
      () => {},
    );
    return run;
  }, []);

  const prepareProject = useCallback(
    async (project: string): Promise<AcquireView | null> => {
      if (remote.current) return null;
      // The user is creating a conversation here, so the project is wanted now.
      // Recording it before the answer keeps a late failure reportable and stops
      // the next decision from acquiring it a second time.
      wanted.current.add(project);
      owned.current.add(project);
      // Supersede any earlier request, exactly as an explicit retry does.
      const seq = (requestSeq.current.get(project) ?? 0) + 1;
      requestSeq.current.set(project, seq);
      return enqueue(() => acquireOnce(project, seq)).catch(() => null);
    },
    [acquireOnce, enqueue],
  );

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

    // Supersede before queueing, not when the chain reaches this decision: a
    // retry must invalidate the answer already on the wire, or a late reply from
    // the previous attempt lands after the retry and describes a request the user
    // has replaced.
    const queuedSeq = new Map<string, number>();
    for (const project of decision.acquire) {
      const seq = (requestSeq.current.get(project) ?? 0) + 1;
      requestSeq.current.set(project, seq);
      queuedSeq.set(project, seq);
    }

    // Reserve the *intent*, not a lease: a second run before the first finishes
    // must not decide to release a project whose acquire is still in flight. The
    // reservation is released again if the answer is not a held lease.
    for (const project of decision.acquire) owned.current.add(project);
    for (const project of decision.release) owned.current.delete(project);

    void enqueue(async () => {
      for (const project of decision.acquire) {
        const seq = queuedSeq.get(project) ?? 0;
        const view = await acquireOnce(project, seq);
        // The retry marker is per-decision, but the sentence is chosen from the
        // state table; a second failure after a retry says so.
        if (!view && retried.has(project)) {
          noteIssue(project, {
            project,
            requestSeq: seq,
            stage: "acquire",
            reason: `${ACQUIRE_FAILED_REASON}（已重试仍失败）`,
          });
        }
      }
      for (const project of decision.release) {
        // Only a project whose acquire reported a lease can be released. One whose
        // acquire is still in flight is handled when its answer arrives: releasing
        // now would either fail or race a lease that is about to be granted.
        if (!held.current.has(project) && !scopes.current.has(project)) continue;
        // A key may be missing because the acquire's lookup failed. Ask the shell
        // now rather than leaving a real lease unreleased; inventing one from the
        // path would ask it to act on a scope it never resolved.
        let scopeKey = scopes.current.get(project) ?? null;
        if (!scopeKey) {
          scopeKey = await invoke<string | null>("runtime_scope_for_project", {
            project,
          }).catch((): string | null => null);
        }
        if (!scopeKey) continue;
        try {
          await invoke("runtime_release_project", { scopeKey });
          scopes.current.delete(project);
          held.current.delete(project);
        } catch {
          // Releasing is best-effort here; the quit drain is the authoritative
          // path and reports its own failures.
        }
      }
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [deps.activeProject, deps.revision, retryTick]);

  return { issues, retryProject, prepareProject };
}
