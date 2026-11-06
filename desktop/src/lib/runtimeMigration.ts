/**
 * The legacy-runtime migration dialog's state machine.
 *
 * Migration is the one action that stops another runtime, so three rules decide its
 * shape, and each one rules out a tempting shortcut:
 *
 * 1. **It never goes through `RpcClient`.** A confirmation replayed by the RPC
 *    client's reconnect machinery would stop a second runtime nobody approved.
 *    Preview and confirm are native commands, called at most once.
 * 2. **The operation id exists before the click.** If the confirmation's answer is
 *    lost, the recovery is to ask about *that id* -- never to click again. A second
 *    click is a second migration.
 * 3. **Cancel is a no-op on the runtime.** A preview is read-only, so cancelling
 *    drops the token here and sends nothing.
 *
 * An unidentified workload is reported as "unverified", never as "idle": saying
 * nothing is running is what invites a migration that interrupts real work.
 */

/** How often to re-read an operation while a migration runs. */
export const MIGRATION_POLL_MS = 1_000;

/** One project the migration would affect. */
export interface AffectedProject {
  scopeKey: string;
  /** What the probe learned about its workload: never assume zero. */
  workload: "verifiedIdle" | "running" | "unverified";
}

/** The read-only statement the user is asked to approve. */
export interface MigrationPreviewView {
  token: string;
  evidenceDigest: string;
  scopeKey: string;
  affected: AffectedProject[];
  blockers: string[];
  expiresAtUnix: number;
}

/** Native progress for one migration operation. */
export interface MigrationOperationView {
  operationId: string;
  phase: string;
  detail: string | null;
}

/** The native migration surface. Deliberately separate from the switch methods. */
export interface MigrationControl {
  preview(scopeKey: string): Promise<MigrationPreviewView>;
  confirm(
    scopeKey: string,
    token: string,
    operationId: string,
    /** Explicit temporary use is scoped to this identity and session only. */
    temporaryUse: boolean,
  ): Promise<MigrationOperationView>;
  operation(scopeKey: string, operationId: string): Promise<MigrationOperationView>;
}

export type MigrationPhase =
  | "idle"
  | "previewing"
  | "awaiting-confirmation"
  | "confirming"
  | "migrating"
  | "finished"
  | "unknown";

export interface MigrationState {
  preview: MigrationPreviewView | null;
  operation: MigrationOperationView | null;
  phase: MigrationPhase;
  error: string | null;
  /** Chosen before the click, so a lost answer is reconciled rather than repeated. */
  operationId: string | null;
  busy: boolean;
}

export interface MigrationDeps {
  control: MigrationControl;
  /** Injected so tests are deterministic and the shell can persist the id. */
  newOperationId?: () => string;
  setTimeoutFn?: (fn: () => void, ms: number) => unknown;
  clearTimeoutFn?: (handle: unknown) => void;
}

/**
 * What the dialog may say about one affected project.
 *
 * Two different sentences, because they are two different decisions: "this project
 * has no work" is a reason to proceed, and "we could not tell" is a reason to ask
 * somebody who knows.
 */
export function describeWorkload(workload: AffectedProject["workload"]): string {
  switch (workload) {
    case "verifiedIdle":
      return "no work is running in this project";
    case "running":
      return "work is running in this project and will be interrupted";
    case "unverified":
    default:
      return "the workload in this project could not be verified";
  }
}

/** Whether the preview blocks confirmation, and why. */
export function migrationBlockers(preview: MigrationPreviewView): string[] {
  const blockers = [...preview.blockers];
  if (preview.affected.some((project) => project.workload === "unverified")) {
    blockers.push("at least one affected project's workload could not be verified");
  }
  return blockers;
}

export function createMigration(scopeKey: string, deps: MigrationDeps) {
  const newOperationId =
    deps.newOperationId ??
    (() =>
      globalThis.crypto?.randomUUID?.() ??
      `mig-${Date.now()}-${Math.random().toString(16).slice(2)}`);
  const setTimer =
    deps.setTimeoutFn ?? ((fn: () => void, ms: number) => globalThis.setTimeout(fn, ms));
  const clearTimer =
    deps.clearTimeoutFn ?? ((handle: unknown) => globalThis.clearTimeout(handle as number));

  let state: MigrationState = {
    preview: null,
    operation: null,
    phase: "idle",
    error: null,
    operationId: null,
    busy: false,
  };
  let disposed = false;
  let pollTimer: unknown = null;
  /** Bumped on every new intent so a late answer cannot overwrite a newer one. */
  let attempt = 0;
  const listeners = new Set<(state: MigrationState) => void>();

  const emit = () => {
    if (disposed) return;
    for (const listener of [...listeners]) listener(state);
  };
  const update = (patch: Partial<MigrationState>) => {
    state = { ...state, ...patch };
    emit();
  };
  const stopPolling = () => {
    if (pollTimer !== null) {
      clearTimer(pollTimer);
      pollTimer = null;
    }
  };

  async function prepare(): Promise<void> {
    if (disposed || state.busy) return;
    attempt += 1;
    const mine = attempt;
    stopPolling();
    update({ phase: "previewing", busy: true, error: null, preview: null, operationId: null });
    try {
      const preview = await deps.control.preview(scopeKey);
      if (disposed || mine !== attempt) return;
      update({ preview, phase: "awaiting-confirmation", busy: false });
    } catch (error) {
      if (disposed || mine !== attempt) return;
      update({ phase: "unknown", busy: false, error: messageOf(error) });
    }
  }

  /** Drops the preview. Calls nothing on the runtime: a preview is read-only. */
  function cancel(): void {
    if (disposed) return;
    attempt += 1;
    stopPolling();
    update({
      preview: null,
      operation: null,
      operationId: null,
      phase: "idle",
      busy: false,
      error: null,
    });
  }

  async function confirm(temporaryUse = false): Promise<void> {
    if (disposed || state.busy) return;
    const preview = state.preview;
    if (!preview) return;
    if (migrationBlockers(preview).length > 0) {
      update({ error: "this migration has unresolved blockers" });
      return;
    }
    attempt += 1;
    const mine = attempt;
    // Chosen before the send, so a lost answer is reconciled by id.
    const operationId = newOperationId();
    update({ phase: "confirming", busy: true, error: null, operationId });
    try {
      const operation = await deps.control.confirm(
        scopeKey,
        preview.token,
        operationId,
        temporaryUse,
      );
      if (disposed || mine !== attempt) return;
      applyOperation(operation, "migrating");
    } catch (error) {
      if (disposed || mine !== attempt) return;
      // The confirmation may have landed. `unknown` keeps the id so the dialog can
      // poll it; confirming again is forbidden.
      update({ phase: "unknown", busy: false, error: messageOf(error) });
      schedulePoll(operationId);
    }
  }

  function applyOperation(operation: MigrationOperationView, phase: MigrationPhase): void {
    const terminal = operation.phase === "completed" || operation.phase === "failed";
    update({
      operation,
      phase: terminal ? "finished" : phase,
      busy: false,
    });
    if (!terminal) schedulePoll(operation.operationId);
  }

  function schedulePoll(operationId: string): void {
    stopPolling();
    pollTimer = setTimer(() => {
      pollTimer = null;
      void poll(operationId);
    }, MIGRATION_POLL_MS);
  }

  async function poll(operationId: string): Promise<void> {
    if (disposed) return;
    const mine = attempt;
    try {
      const operation = await deps.control.operation(scopeKey, operationId);
      if (disposed || mine !== attempt) return;
      applyOperation(operation, "migrating");
    } catch {
      if (disposed || mine !== attempt) return;
      // A read that failed is not evidence the migration failed: ask again about
      // the same id rather than start over.
      schedulePoll(operationId);
    }
  }

  return {
    getState: () => state,
    subscribe(listener: (state: MigrationState) => void) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    prepare,
    cancel,
    confirm,
    dispose() {
      disposed = true;
      stopPolling();
      listeners.clear();
    },
  };
}

function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/**
 * What the sidecar sends on the wire.
 *
 * The view shapes above are the UI's vocabulary; these are the wire's. They are kept
 * apart because Rust serialises a `MigrationWorkload` as an externally tagged enum
 * (`{"unverified": "reason"}`), and pretending otherwise would either flatten an
 * unverifiable workload into "idle" or require the sidecar to learn a second
 * spelling of its own module.
 */
interface WireAffected {
  scope_key: string;
  workload: "verified_idle" | "running" | { unverified: string };
}

interface WirePreview {
  token: string;
  evidence_digest: string;
  scope_key: string;
  affected: WireAffected[];
  blockers: string[];
  expires_at_unix: number;
}

interface WireOperation {
  operation_id: string;
  phase: string;
  detail: string | null;
}

/** One affected project, with the reason an unverified workload could not be proven. */
export function toAffectedProject(wire: WireAffected): AffectedProject & { detail?: string } {
  if (typeof wire.workload === "string") {
    return {
      scopeKey: wire.scope_key,
      workload: wire.workload === "verified_idle" ? "verifiedIdle" : "running",
    };
  }
  // The reason travels with the workload: "could not be verified" alone leaves the
  // user unable to tell whether to wait, close something, or ask a human.
  return { scopeKey: wire.scope_key, workload: "unverified", detail: wire.workload.unverified };
}

export function toPreviewView(wire: WirePreview): MigrationPreviewView {
  return {
    token: wire.token,
    evidenceDigest: wire.evidence_digest,
    scopeKey: wire.scope_key,
    affected: wire.affected.map(toAffectedProject),
    blockers: wire.blockers,
    expiresAtUnix: wire.expires_at_unix,
  };
}

export function toOperationView(wire: WireOperation): MigrationOperationView {
  return {
    operationId: wire.operation_id,
    phase: wire.phase,
    detail: wire.detail,
  };
}

/**
 * The desktop control, over the shell's own commands.
 *
 * Never `RpcClient`: a confirmation replayed by the reconnect machinery would stop a
 * second runtime nobody approved. Every call here is a single attempt, and the shell
 * does not resend one either.
 */
export function localMigrationControl(): MigrationControl {
  return {
    async preview(scopeKey) {
      const { invoke } = await import("@tauri-apps/api/core");
      const wire = (await invoke("runtime_migration_preview", { scopeKey })) as WirePreview;
      return toPreviewView(wire);
    },
    async confirm(scopeKey, token, operationId, temporaryUse) {
      const { invoke } = await import("@tauri-apps/api/core");
      const wire = (await invoke("runtime_migration_confirm", {
        scopeKey,
        token,
        operationId,
        temporaryUse,
      })) as WireOperation;
      return toOperationView(wire);
    },
    async operation(scopeKey, operationId) {
      const { invoke } = await import("@tauri-apps/api/core");
      const wire = (await invoke("runtime_migration_operation", {
        scopeKey,
        operationId,
      })) as WireOperation | null;
      if (wire === null) {
        // No record is not "it did not happen": the sidecar may have restarted. The
        // state machine treats a throw as "ask again", which is the honest answer.
        throw new Error("the sidecar has no record of this migration");
      }
      return toOperationView(wire);
    },
  };
}
