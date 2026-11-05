/**
 * The App's side of runtime version lifecycle: what to show, and when it is safe
 * to act.
 *
 * Four rules shape this file, and each one rules out the obvious shortcut:
 *
 * 1. **The control path is not the business RPC path.** Preview and confirm go
 *    through their own Tauri commands, not `RpcClient`. Wrapping them in the RPC
 *    client's reconnect-and-replay machinery would silently re-send a
 *    confirmation after a reconnect -- i.e. perform a restart the user may never
 *    have approved twice.
 * 2. **An operation id exists before the click.** If the answer to a confirmation
 *    is lost, the recovery is to ask about *that id*, never to click again. A
 *    second click would be a second restart.
 * 3. **Unknown is not "fine".** A runtime whose build cannot be verified gets a
 *    banner that says so, and no restart button. A legacy runtime says its exit
 *    guarantee is not in effect. Neither is allowed to claim compatibility.
 * 4. **"Not found" is not "did not happen".** The runtime may have restarted
 *    since; the operation id then belongs to a process that is gone. That is
 *    "unknown", and the UI must ask the user to refresh rather than assert a
 *    failure or start over.
 *
 * Cancelling a preview calls nothing on the runtime: a preview is read-only, so
 * "cancel" only drops the token here.
 */

import type { Compatibility } from "./generated/Compatibility";
import type { OperationStatus } from "./generated/OperationStatus";
import type { RestartPreview } from "./generated/RestartPreview";
import type { RuntimeStatusView } from "./generated/RuntimeStatusView";

export type { Compatibility, OperationStatus, RestartPreview, RuntimeStatusView };

/**
 * The local control surface.
 *
 * Every method takes the scope key the *shell* resolved: the key is a canonical
 * runtime directory, and a WebView that invented one would name a scope the shell
 * has never opened. The shell refuses those, which is why the error type below has
 * a distinct "not open" case.
 */
export interface RuntimeControl {
  status(scopeKey: string): Promise<RuntimeStatusView>;
  preview(scopeKey: string): Promise<RestartPreview>;
  confirm(scopeKey: string, token: string, operationId: string): Promise<OperationStatus>;
  operation(scopeKey: string, operationId: string): Promise<OperationStatus>;
}

/** Raised when this client cannot control the runtime at all (the remote build). */
export class UnsupportedLocalControl extends Error {
  constructor(message = "runtime restart is only available from the desktop app") {
    super(message);
    this.name = "UnsupportedLocalControl";
  }
}

/** How often to re-read status while the banner is visible. */
export const STATUS_POLL_MS = 5_000;

/**
 * How often to re-read an operation while one is in flight.
 *
 * Faster than the idle poll because this is the user watching their own action
 * complete. Still a *read*: the poll must not become a substitute for the lease
 * heartbeat, which is a separate connection with its own cadence.
 */
export const OPERATION_POLL_MS = 1_000;

/** The confirmation's own bound, matching the shell's `SWITCH_BUDGET`. */
export const CONFIRM_BUDGET_MS = 10_000;

/** One client, as the banner counts it. */
export interface ClientCounts {
  /** Clients other than this App. */
  others: number;
  /** Clients other than this App that are actually running tasks. */
  othersRunning: number;
}

/**
 * Counts the clients a restart would interrupt, excluding this App's own row.
 *
 * "2 TUIs" is what the user is being asked to approve, so this App must not be
 * counted among the affected: a banner that says "3 clients will be interrupted"
 * when two of them are the window in front of you is asking the wrong question.
 * Clients are already deduplicated by `clientRunId` on the runtime side, so a
 * reconnecting TUI is one row, not two.
 */
export function countOtherClients(
  status: RuntimeStatusView,
  selfClientRunId: string | null,
): ClientCounts {
  const others = status.clients.filter(
    (client) => selfClientRunId === null || client.clientRunId !== selfClientRunId,
  );
  return {
    others: others.length,
    othersRunning: others.filter((client) => client.mode === "business").length,
  };
}

/**
 * Whether a restart needs a second confirmation.
 *
 * The runtime answers this in the preview, and this function only adds what the
 * preview cannot know: an incomplete impact count must be treated as "there may be
 * more", so it always requires confirmation. `status.clients` is one page, so
 * `counts.clients` -- the whole-runtime number -- is what decides "just me".
 */
export function needsSecondConfirmation(preview: RestartPreview): boolean {
  // Incomplete counts are lower bounds, so a zero proves nothing: treat the
  // preview as "there may be more" and ask.
  if (!preview.counts.complete) return true;
  if (preview.requiresSecondConfirmation) return true;
  const counts = preview.counts;
  return (
    counts.clients > 1 ||
    counts.pendingClients > 0 ||
    counts.running > 0 ||
    counts.queued > 0 ||
    counts.plugins > 0
  );
}

/** Whether this compatibility class is shown at all. */
function shouldShowBannerRaw(status: RuntimeStatusView): boolean {
  return status.compatibility !== "sameBuild";
}

/** Everything the banner renders, derived so the component has no policy. */
export interface StatusSummary {
  /** What the user sees as "current". */
  currentVersion: string | null;
  /** What the user sees as "expected". */
  expectedVersion: string | null;
  /** True only when comparable version evidence says the runtime is older. */
  currentIsOlder: boolean;
  /** The banner is shown at all. */
  visible: boolean;
  /** A restart button may be offered. */
  canRestart: boolean;
  /** Why not, when it cannot. */
  cannotRestartReason: string | null;
  /** The sentence(s) to render. */
  message: string;
}

/**
 * Turns a status view into what the banner shows.
 *
 * "Different build" is never called "old version" unless there is comparable
 * release evidence: a rebuild at the same version, or a dirty local build, is
 * "different", and telling the user to "upgrade" from a newer build is worse than
 * saying nothing.
 */
export function summarise(status: RuntimeStatusView): StatusSummary {
  const current = status.identity.daemonDisplayVersion ?? status.identity.guardianDisplayVersion;
  const expected = status.expectedDisplayVersion;
  const visible = shouldShowBannerRaw(status);
  const comparable =
    current !== null && expected !== null && /^\d/.test(current) && /^\d/.test(expected);

  const currentIsOlder =
    comparable && compareVersions(current as string, expected as string) < 0;

  const cannotRestartReason =
    // A legacy runtime is not ours to switch: it may be shared with a version of
    // yi-agent that knows nothing about this protocol, so "restart" would mean
    // killing someone else's process.
    status.compatibility === "legacyTemporaryChoiceRequired" || !status.lifecycleManaged
    ? "this runtime was not started by the version of yi-agent that manages restarts"
    : !status.localRestartAvailable
      ? "this app cannot restart the runtime from here"
      : status.expectedBuildId === null
        ? "this build cannot verify the runtime it would switch to"
        : null;

  return {
    currentVersion: current,
    expectedVersion: expected,
    currentIsOlder,
    visible,
    canRestart: visible && cannotRestartReason === null,
    cannotRestartReason,
    message: describeStatus(status),
  };
}

/** Compares dotted numeric versions; non-numeric tails compare as strings. */
export function compareVersions(a: string, b: string): number {
  const split = (value: string): string[] =>
    value
      .split(/[.+-]/)
      .filter((part) => part.length > 0);
  const left = split(a);
  const right = split(b);
  for (let index = 0; index < Math.max(left.length, right.length); index += 1) {
    const one = left[index];
    const two = right[index];
    if (one === undefined) return -1;
    if (two === undefined) return 1;
    const oneNumber = Number(one);
    const twoNumber = Number(two);
    if (Number.isFinite(oneNumber) && Number.isFinite(twoNumber)) {
      if (oneNumber !== twoNumber) return oneNumber < twoNumber ? -1 : 1;
      continue;
    }
    if (one !== two) return one < two ? -1 : 1;
  }
  return 0;
}

/**
 * The sentence the banner shows, per compatibility class.
 *
 * Each class gets its own sentence because each is a different decision for the
 * user: "different build" is informational, "control only" means tasks are
 * already disabled, "legacy" means the exit guarantee they were promised does not
 * apply, and "unknown" means nothing may be assumed.
 */
export function describeStatus(status: RuntimeStatusView): string {
  switch (status.compatibility) {
    case "sameBuild":
      return "the background runtime matches this app";
    case "compatibleMismatch":
      return "this project's background runtime is a different build from the app; some new features may be unavailable";
    case "controlOnly":
      return "the background runtime's protocol is incompatible; tasks are disabled until it is restarted";
    case "legacyTemporaryChoiceRequired":
      return "this background runtime was started by an older version: background exit management is not in effect for it";
    case "unknown":
    default:
      return (
        status.message ??
        "the background runtime could not be identified; no business operation will be allowed until it is"
      );
  }
}

/**
 * Whether the version banner is worth showing for this status.
 *
 * A runtime that could not be identified is exactly when the user must be told, so
 * `unknown` is visible too. Only a verified match and a verified-compatible
 * different build with nothing to say are quiet.
 */
export function shouldShowBanner(status: RuntimeStatusView): boolean {
  return summarise(status).visible;
}

// ---------------------------------------------------------------------------
// The lifecycle state machine
// ---------------------------------------------------------------------------

/** What the banner is currently doing. */
export type Phase =
  | "idle"
  /** A preview is being read. */
  | "previewing"
  /** A preview is on screen and the user is deciding. */
  | "awaiting-confirmation"
  /** The confirmation is in flight. */
  | "confirming"
  /** The switch is running. */
  | "switching"
  /** The switch reported a terminal outcome. */
  | "finished"
  /** The outcome cannot be determined; the user must refresh. */
  | "unknown";

export interface LifecycleState {
  status: RuntimeStatusView | null;
  preview: RestartPreview | null;
  operation: OperationStatus | null;
  phase: Phase;
  /** A sentence for a failed or refused step. */
  error: string | null;
  /** The operation id chosen before the click, kept for recovery. */
  operationId: string | null;
  /** True while a request is in flight; every button is disabled. */
  busy: boolean;
  /**
   * True when this scope cannot use the version-switch control surface at all
   * (a runtime that predates it). The banner still reports the version; the
   * restart affordance is replaced with an explanation, not hidden, because "no
   * button" reads as "nothing to do".
   */
  controlUnavailable: boolean;
}

export interface Deps {
  control: RuntimeControl;
  /** Called with the id chosen before the click, so the shell can persist it. */
  onOperationChosen?: (scopeKey: string, operationId: string) => void;
  /** Generates the operation id. Injected so tests can be deterministic. */
  newOperationId?: () => string;
  /** Reads the id the shell remembers for this scope, after a reload. */
  rememberedOperationId?: (scopeKey: string) => Promise<string | null>;
  /** Injected timers so tests never wait on wall clock. */
  setTimeoutFn?: (fn: () => void, ms: number) => unknown;
  clearTimeoutFn?: (handle: unknown) => void;
}

/**
 * The whole lifecycle as one object with methods, so the React hook stays a thin
 * adapter and every transition is testable without a DOM.
 *
 * The scope key is captured at construction. Every async continuation re-checks it
 * against the live key before writing state: a project switch while a preview is
 * in flight must not let the old project's token land in the new project's
 * confirmation.
 */
export function createRuntimeLifecycle(scopeKey: string, deps: Deps) {
  const newOperationId =
    deps.newOperationId ??
    (() =>
      globalThis.crypto?.randomUUID?.() ??
      `op-${Date.now()}-${Math.random().toString(16).slice(2)}`);
  const setTimer =
    deps.setTimeoutFn ?? ((fn: () => void, ms: number) => globalThis.setTimeout(fn, ms));
  const clearTimer =
    deps.clearTimeoutFn ?? ((handle: unknown) => globalThis.clearTimeout(handle as number));

  let state: LifecycleState = {
    status: null,
    preview: null,
    operation: null,
    phase: "idle",
    error: null,
    operationId: null,
    busy: false,
    controlUnavailable: false,
  };
  let disposed = false;
  let pollTimer: unknown = null;
  /** Bumped on every new preview/confirm so a late answer cannot overwrite. */
  let attempt = 0;

  const listeners = new Set<(state: LifecycleState) => void>();

  const emit = () => {
    if (disposed) return;
    for (const listener of [...listeners]) listener(state);
  };
  const update = (patch: Partial<LifecycleState>) => {
    state = { ...state, ...patch };
    emit();
  };

  const stopPolling = () => {
    if (pollTimer !== null) {
      clearTimer(pollTimer);
      pollTimer = null;
    }
  };

  const scheduleStatus = (ms: number) => {
    stopPolling();
    pollTimer = setTimer(() => {
      pollTimer = null;
      void refresh();
    }, ms);
  };

  const scheduleOperation = (operationId: string, ms: number) => {
    stopPolling();
    pollTimer = setTimer(() => {
      pollTimer = null;
      void pollOperation(operationId);
    }, ms);
  };

  /** One status read. Errors are shown, not thrown: the banner keeps its place. */
  async function refresh(): Promise<void> {
    if (disposed) return;
    const mine = attempt;
    try {
      const status = await deps.control.status(scopeKey);
      if (disposed || mine !== attempt) return;
      update({ status });
      // An operation the runtime is running is also visible from status; adopt it
      // so a reloaded WebView resumes the same progress instead of showing "idle".
      if (!state.operationId && status.activeOperation) {
        update({ operationId: status.activeOperation.operationId, operation: status.activeOperation, phase: "switching" });
      }
    } catch (error) {
      if (disposed || mine !== attempt) return;
      update({ error: messageOf(error), controlUnavailable: isControlUnavailable(error) });
    }
    if (!disposed) {
      const delay = state.phase === "switching" ? OPERATION_POLL_MS : STATUS_POLL_MS;
      scheduleStatus(delay);
    }
  }

  /** Reads one preview. Read-only: nothing on the runtime changes. */
  async function prepare(): Promise<void> {
    if (disposed || state.busy) return;
    attempt += 1;
    const mine = attempt;
    update({ busy: true, error: null, phase: "previewing", preview: null });
    try {
      const preview = await deps.control.preview(scopeKey);
      if (disposed || mine !== attempt) return;
      update({ preview, phase: "awaiting-confirmation", busy: false });
    } catch (error) {
      if (disposed || mine !== attempt) return;
      update({
        busy: false,
        phase: "idle",
        error: messageOf(error),
        controlUnavailable: isControlUnavailable(error),
      });
    }
  }

  /** Drops the preview. Never calls the runtime: a preview changed nothing. */
  function cancel(): void {
    if (disposed) return;
    attempt += 1;
    update({ preview: null, phase: "idle", busy: false, error: null });
  }

  /**
   * Consumes the preview token.
   *
   * The id is chosen and handed to the shell *before* the request leaves, so a
   * lost answer is recovered by polling that id. A stale token means the impact
   * changed while the dialog was open: the only correct response is to preview
   * again and ask again -- never to confirm the new impact on the old approval.
   */
  async function confirm(): Promise<void> {
    if (disposed || state.busy) return;
    const preview = state.preview;
    if (!preview) return;
    if (!preview.canRestart) {
      update({ error: preview.blockedReason ?? "this runtime cannot be restarted automatically" });
      return;
    }
    const operationId = newOperationId();
    attempt += 1;
    const mine = attempt;
    update({ busy: true, error: null, phase: "confirming", operationId });
    deps.onOperationChosen?.(scopeKey, operationId);
    try {
      const confirmed = await deps.control.confirm(scopeKey, preview.token, operationId);
      if (disposed || mine !== attempt) return;
      update({ operation: confirmed, phase: "switching", busy: false, preview: null });
      scheduleOperation(confirmed.operationId, OPERATION_POLL_MS);
    } catch (error) {
      if (disposed || mine !== attempt) return;
      // A lost answer is not a failed action: keep the id and poll it. The shell
      // also remembers it, so a reload can continue.
      update({ busy: false, phase: "unknown", error: messageOf(error) });
      scheduleOperation(operationId, OPERATION_POLL_MS);
    }
  }

  /** One operation read. `NotImplemented` from the shell means "unknown", not "failed". */
  async function pollOperation(operationId: string): Promise<void> {
    if (disposed) return;
    const mine = attempt;
    try {
      const status = await deps.control.operation(scopeKey, operationId);
      if (disposed || mine !== attempt) return;
      const terminal = isTerminal(status.phase);
      update({
        operation: status,
        phase: terminal ? terminalPhase(status.phase) : "switching",
      });
      if (!terminal) {
        scheduleOperation(operationId, OPERATION_POLL_MS);
        return;
      }
      await refresh();
      return;
    } catch (error) {
      if (disposed || mine !== attempt) return;
      const kind = kindOf(error);
      if (kind === "not_found" || kind === "timed_out") {
        // "Not found" does not mean "did not run": the runtime may have restarted
        // since. The honest answer is that we cannot tell, and the user refreshes.
        update({
          phase: "unknown",
          error:
            kind === "not_found"
              ? "the runtime no longer knows this operation; it may have restarted — refresh to see the current version"
              : "the runtime did not answer in time; refresh to see the current version",
        });
        return;
      }
      update({ error: messageOf(error) });
      scheduleOperation(operationId, OPERATION_POLL_MS);
    }
  }

  /** Resumes the operation the shell remembers for this scope, if any. */
  async function resume(): Promise<void> {
    const remembered = await deps.rememberedOperationId?.(scopeKey);
    if (disposed || !remembered) return;
    update({ operationId: remembered, phase: "switching" });
    await pollOperation(remembered);
  }

  return {
    getState: () => state,
    subscribe(listener: (next: LifecycleState) => void): () => void {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
    start(): void {
      void refresh();
    },
    refresh,
    prepare,
    confirm,
    cancel,
    resume,
    dispose(): void {
      disposed = true;
      stopPolling();
      listeners.clear();
    },
  };
}

export type RuntimeLifecycle = ReturnType<typeof createRuntimeLifecycle>;

/** Phases that will not change on their own. */
export function isTerminal(phase: string): boolean {
  return (
    phase === "ready" ||
    phase === "failed" ||
    phase === "manual_action_required" ||
    phase === "stopped_no_clients"
  );
}

function terminalPhase(phase: string): Phase {
  return phase === "ready" ? "finished" : "finished";
}

/**
 * Maps a control error to a coarse kind the UI reasons about.
 *
 * The shell's messages are the only source of this distinction (there is no error
 * code on the command boundary), so the mapping is deliberately narrow: anything
 * unrecognised stays a plain failure and is retried, while the two cases that must
 * *not* be retried are named explicitly.
 */
export function kindOf(error: unknown): "not_found" | "timed_out" | "refused" | "other" {
  const text = messageOf(error).toLowerCase();
  if (text.includes("no longer knows") || text.includes("not found")) return "not_found";
  if (text.includes("timed out") || text.includes("did not finish")) return "timed_out";
  if (text.includes("refus") || text.includes("not open in this app") || text.includes("no live lease")) {
    return "refused";
  }
  return "other";
}

/**
 * Whether a status read failed because this runtime has no control surface.
 *
 * The temporary fallback path: `PROTOCOL_VERSION` accepts `CURRENT-1`, so a runtime
 * one minor version behind is a supported peer that simply has no `runtime.*`
 * commands. Telling the user to "upgrade the runtime" is right; telling them the
 * runtime is broken is not. When the control surface reaches stable and older
 * minors are dropped, this branch and `run_control`'s `NotImplemented` sibling go
 * away together.
 */
export function isControlUnavailable(error: unknown): boolean {
  const text = messageOf(error).toLowerCase();
  return (
    text.includes("not implemented") ||
    text.includes("unknown command") ||
    text.includes("unsupported") ||
    text.includes("no such command") ||
    text.includes("does not support")
  );
}

function messageOf(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  return "the runtime control request failed";
}

/**
 * The desktop implementation, over the shell's own commands.
 *
 * Deliberately not built on `RpcClient`: these calls must never be queued behind
 * business traffic, replayed after a reconnect, or retried automatically. Each one
 * is a single attempt; recovery is the operation id, not a resend.
 */
export function localRuntimeControl(): RuntimeControl {
  return {
    async status(scopeKey) {
      const { invoke } = await import("@tauri-apps/api/core");
      return (await invoke("runtime_status", { scopeKey })) as RuntimeStatusView;
    },
    async preview(scopeKey) {
      const { invoke } = await import("@tauri-apps/api/core");
      return (await invoke("runtime_preview", { scopeKey })) as RestartPreview;
    },
    async confirm(scopeKey, token, operationId) {
      const { invoke } = await import("@tauri-apps/api/core");
      const result = (await invoke("runtime_confirm", {
        scopeKey,
        token,
        operationId,
      })) as { operationId: string };
      // The shell answers with the id it accepted. Reading status is the caller's
      // job one tick later; inventing a phase here would be a guess.
      return {
        operationId: result.operationId,
        phase: "confirming",
        detail: null,
        readyIdentity: null,
      };
    },
    async operation(scopeKey, operationId) {
      const { invoke } = await import("@tauri-apps/api/core");
      const result = (await invoke("runtime_operation", {
        scopeKey,
        operationId,
      })) as OperationStatus | null;
      if (result === null) {
        throw new Error("the runtime no longer knows this operation");
      }
      return result;
    },
  };
}

/**
 * The remote control surface: status is readable, control is not.
 *
 * A phone can see that its project's runtime is a different build, and it must be
 * able to say so. It cannot restart anything: the control channel is a local unix
 * socket, so offering a button would promise an action the transport cannot carry
 * out. Both mutating methods fail with the same explicit error the banner renders
 * as "do this on the desktop".
 */
export function remoteRuntimeControl(status: (scopeKey: string) => Promise<RuntimeStatusView>): RuntimeControl {
  return {
    status,
    preview: async () => {
      throw new UnsupportedLocalControl();
    },
    confirm: async () => {
      throw new UnsupportedLocalControl();
    },
    operation: async () => {
      throw new UnsupportedLocalControl();
    },
  };
}

/**
 * The fence that names one foreground attempt, as the shell needs it.
 *
 * Every field comes from what the daemon already persisted; a caller that invented
 * an epoch would be describing a conversation that does not exist, and the daemon
 * refuses a stale fence rather than repairing the wrong attempt.
 */
export interface RecoveryTargetView {
  sessionId: string;
  rootId: string;
  attemptId: string;
  generation: string;
  bindingEpoch: number;
  driverEpoch: number;
}

/** The structured refusal the shell reports, shaped by `kind`. */
export type HostRecoveryError =
  | { kind: "rejected"; code: string; reason: string }
  | { kind: "transport"; reason: string }
  | { kind: "protocol"; reason: string }
  | { kind: "sessionNotFound"; reason: string };

/**
 * One sentence per error kind, so a transport failure is never rendered as a
 * refusal. The two need different user actions: "try again" versus "this will never
 * work". The shell's `reason` is deliberately not shown -- it is host text for the
 * log, and the DOM only ever gets the mapped sentence.
 */
export function describeRecoveryError(error: unknown): string {
  const shaped = error as Partial<HostRecoveryError> | null;
  const kind = shaped?.kind;
  switch (kind) {
    case "rejected":
      return "运行时拒绝了这次恢复，请确认该项目仍属于本应用";
    case "transport":
      return "未能联系到运行时，请稍后重试";
    case "protocol":
      return "运行时返回了本版本无法识别的结果";
    case "sessionNotFound":
      return "该项目未在本应用中打开，没有可恢复的运行时";
    default:
      return "恢复请求失败";
  }
}

/**
 * The shell's confirmed foreground-root recovery commands.
 *
 * Separate from `RuntimeControl` on purpose: those calls describe a runtime restart,
 * while these repair one conversation's attempt. Both go through the shell's own
 * lease, and neither is exposed as a model tool -- a repair stops work, so it stays a
 * local window action.
 */
export interface ForegroundRecoveryControl {
  inspect(scopeKey: string, target: RecoveryTargetView): Promise<RecoveryImpactViewFromShell>;
  prepare(
    scopeKey: string,
    target: RecoveryTargetView,
    operationId: string,
  ): Promise<RecoveryOperationView>;
  confirm(
    scopeKey: string,
    target: RecoveryTargetView,
    operationId: string,
    token: string,
  ): Promise<RecoveryOperationView>;
  operation(
    scopeKey: string,
    target: RecoveryTargetView,
    operationId: string,
  ): Promise<RecoveryOperationView | null>;
}

import type {
  RecoveryImpactView as RecoveryImpactViewFromShell,
  RecoveryOperationView,
} from "../components/ForegroundRootRecovery";

/** The desktop implementation, over the shell's `foreground_root_recovery_*` commands. */
export function localForegroundRecovery(): ForegroundRecoveryControl {
  return {
    async inspect(scopeKey, target) {
      const { invoke } = await import("@tauri-apps/api/core");
      return (await invoke("foreground_root_recovery_inspect", {
        scopeKey,
        target,
      })) as RecoveryImpactViewFromShell;
    },
    async prepare(scopeKey, target, operationId) {
      const { invoke } = await import("@tauri-apps/api/core");
      return (await invoke("foreground_root_recovery_prepare", {
        scopeKey,
        target,
        operationId,
      })) as RecoveryOperationView;
    },
    async confirm(scopeKey, target, operationId, token) {
      const { invoke } = await import("@tauri-apps/api/core");
      return (await invoke("foreground_root_recovery_confirm", {
        scopeKey,
        target,
        operationId,
        token,
      })) as RecoveryOperationView;
    },
    async operation(scopeKey, target, operationId) {
      const { invoke } = await import("@tauri-apps/api/core");
      return (await invoke("foreground_root_recovery_operation", {
        scopeKey,
        target,
        operationId,
      })) as RecoveryOperationView | null;
    },
  };
}
