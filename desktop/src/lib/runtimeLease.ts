/**
 * Which project leases the desktop shell should hold, derived from what the user
 * is actually doing.
 *
 * The runtime treats a registration as "a frontend is using this project", so the
 * shell must not register whatever merely appears in the sidebar. A project the
 * user opened last week and never returned to must not keep a daemon alive
 * forever, and a project the user left while it is still working must not be
 * released out from under that work.
 *
 * The rule is therefore exactly two sources of demand:
 *
 * 1. the project of the **currently open** thread (the one the user is looking
 *    at), and
 * 2. any project that has a thread **still working** (`running` or awaiting
 *    approval) — background work is a real reason to stay attached.
 *
 * Everything else is not demand, including "it is in the recent list".
 */

/** Thread states that mean the runtime is still doing work in that project. */
const BUSY: ReadonlySet<string> = new Set(["running", "awaiting_approval"]);

export interface LeaseInput {
  /** The project of the open thread, or null when nothing is open. */
  activeProject: string | null;
  /** Every known thread's project and status (status may be missing). */
  threads: ReadonlyArray<{ cwd: string; status?: string }>;
  /** Projects this run already holds a lease for. */
  owned: ReadonlyArray<string>;
}

export interface LeaseDecision {
  acquire: string[];
  release: string[];
}

/**
 * Normalise a project path for comparison.
 *
 * Two spellings of one directory must compare equal or the shell would acquire
 * one and release the other every time the user switched tabs. Only the trailing
 * slash is removed: deeper canonicalisation (symlinks) is the Rust side's job and
 * must not be re-implemented here with different rules, or the two would disagree
 * about which directory a scope names.
 */
export function normalizeProject(path: string): string {
  const trimmed = path.replace(/\/+$/, "");
  return trimmed === "" ? "/" : trimmed;
}

/**
 * The projects that currently justify a lease, sorted.
 *
 * Exposed separately from [`leaseDecision`] because it doubles as the change
 * signature: the shell should only re-run the decision when this set changes, not
 * on every unrelated re-render or status blip.
 */
export function wantedProjects(input: {
  activeProject: string | null;
  threads: ReadonlyArray<{ cwd: string; status?: string }>;
}): string[] {
  const wanted = new Set<string>();
  if (input.activeProject) wanted.add(normalizeProject(input.activeProject));
  for (const thread of input.threads) {
    if (thread.status && BUSY.has(thread.status)) {
      wanted.add(normalizeProject(thread.cwd));
    }
  }
  return [...wanted].sort();
}

export function leaseDecision(input: LeaseInput): LeaseDecision {
  const wanted = new Set(wantedProjects(input));

  const owned = new Set(input.owned.map(normalizeProject));
  const acquire: string[] = [];
  for (const project of wanted) {
    if (!owned.has(project)) acquire.push(project);
  }
  const release: string[] = [];
  for (const project of owned) {
    if (!wanted.has(project)) release.push(project);
  }
  // Deterministic order: the side effects run in this order, and a stable order
  // makes a log of the switches readable.
  acquire.sort();
  release.sort();
  return { acquire, release };
}
