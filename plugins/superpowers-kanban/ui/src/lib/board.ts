/**
 * The board's own backend adapter.
 *
 * The plugin talks to its backend through `host.request(method, params)`, and
 * the host only forwards methods this plugin's manifest declared. There is no
 * general RPC client here on purpose: a plugin page that could call arbitrary
 * host methods would be a way around the bridge's declared-method check.
 */

import type { PluginHostApi, OperationResult } from "@yi-agent/plugin-sdk";
import { isResultUnknown } from "./errors";
import { METHODS } from "./methods";

/** Raw card shape as it crosses the backend boundary. */
export interface RawCard {
  id?: unknown;
  state?: unknown;
  spec_path?: unknown;
  plan_path?: unknown;
  [key: string]: unknown;
}

export interface SwitchState {
  on: boolean;
  source: "project" | "global" | "default";
}

/** Which layer supplied the effective switch value. */
export type SwitchSource = SwitchState["source"];

export interface BoardSettings {
  default_max_tasks: number;
  interval_secs: number;
  windows: KanbanWindow[];
}

export interface KanbanWindow {
  days: string;
  start: string;
  end: string;
  all_day: boolean;
  max_tasks: number;
}

/** Read every card this project's board holds. */
export async function listCards(host: PluginHostApi): Promise<RawCard[]> {
  const result = await host.request<{ cards?: RawCard[] }>(METHODS.list, {});
  return Array.isArray(result?.cards) ? result.cards : [];
}

/** Read the two-layer switch and where its value came from. */
export async function readSwitch(host: PluginHostApi): Promise<SwitchState> {
  const result = await host.request<{ on?: unknown; source?: unknown }>(
    METHODS.switchRead,
    {},
  );
  const source =
    result?.source === "project" || result?.source === "global" || result?.source === "default"
      ? result.source
      : "default";
  return { on: result?.on === true, source };
}

export async function writeSwitch(host: PluginHostApi, on: boolean): Promise<void> {
  await requestWrite(host, METHODS.switchWrite, { on });
}

export async function enqueueCard(
  host: PluginHostApi,
  specPath: string,
  planPath: string,
): Promise<void> {
  await requestWrite(host, METHODS.enqueue, { spec_path: specPath, plan_path: planPath });
}

/**
 * Deliver a card that has only a title.
 *
 * A blank title is refused here rather than sent: the backend refuses it too,
 * but making the round trip to learn that only delays the same message.
 */
export async function enqueuePrequeuedCard(
  host: PluginHostApi,
  title: string,
  originThreadId: string | null,
): Promise<void> {
  const trimmed = title.trim();
  if (trimmed === "") throw new Error("a prequeued card needs a non-empty title");
  const params: Record<string, unknown> = { title: trimmed };
  if (originThreadId !== null && originThreadId.trim() !== "") {
    params.origin_thread_id = originThreadId;
  }
  await requestWrite(host, METHODS.enqueuePrequeued, params);
}

/** Give a pre-queued card its spec and plan so the backend can promote it. */
export async function promoteCard(
  host: PluginHostApi,
  cardId: string,
  specPath: string,
  planPath: string,
): Promise<void> {
  await requestWrite(host, METHODS.promote, {
    id: cardId,
    spec_path: specPath,
    plan_path: planPath,
  });
}

export async function readSettings(host: PluginHostApi): Promise<BoardSettings> {
  const result = await host.request<{ settings?: BoardSettings }>(METHODS.settingsRead, {});
  if (!result?.settings) throw new Error("the backend returned no settings");
  return result.settings;
}

/** Replace the whole settings document; the backend is the normalizing authority. */
export async function writeSettings(
  host: PluginHostApi,
  settings: BoardSettings,
): Promise<void> {
  await requestWrite(host, METHODS.settingsWrite, { settings });
}

/**
 * Ask the backend what became of a write whose reply never arrived.
 *
 * The protocol forbids treating "no answer" as failure and retrying (G1 §9.3):
 * the write may have committed. The backend keeps a ledger keyed by the write's
 * own `request_id`, so the honest move is to look that id up rather than guess.
 *
 * Two doors lead to the same ledger. The host's generic `result()` is the
 * preferred one, but a host that does not implement it answers `NotSupported` —
 * in that case the plugin's own declared `board/operation` method reads the
 * same ledger. Falling through matters: without it, every unknown write on such
 * a host would stay unknown even though the answer is one call away.
 *
 * A `pending` verdict is reported as unknown, not as success: the backend is
 * still deciding, and the caller must refresh rather than assume.
 */
export async function settleWrite(
  host: PluginHostApi,
  requestId: string | undefined,
): Promise<"committed" | "rejected" | "unknown"> {
  if (requestId === undefined || requestId === "") return "unknown";
  const viaHost = await askHostResult(host, requestId);
  if (viaHost !== null) return viaHost;
  return askBoardOperation(host, requestId);
}

/** The host's generic ledger query, or `null` when this host cannot answer it. */
async function askHostResult(
  host: PluginHostApi,
  requestId: string,
): Promise<"committed" | "rejected" | "unknown" | null> {
  let result: OperationResult;
  try {
    result = await host.result(requestId);
  } catch {
    // The query itself failing says nothing about the original write, and it
    // may mean this host simply has no such method. Try the plugin's own door.
    return null;
  }
  if (result?.status === "committed") return "committed";
  if (result?.status === "rejected") return "rejected";
  return "unknown";
}

/**
 * Read the ledger through `board/operation`.
 *
 * Declared locally rather than imported as an SDK type: the backend serialises
 * `request_id` (snake_case), and borrowing the SDK's `OperationResult` would
 * bake in a field-name assumption this plugin does not own. Only `status` is
 * read, because that is all the caller acts on.
 */
async function askBoardOperation(
  host: PluginHostApi,
  requestId: string,
): Promise<"committed" | "rejected" | "unknown"> {
  try {
    const result = await host.request<{ status?: unknown }>(METHODS.operation, {
      request_id: requestId,
    });
    if (result?.status === "committed") return "committed";
    if (result?.status === "rejected") return "rejected";
    return "unknown";
  } catch {
    // Neither door answered. That is still not evidence the write failed.
    return "unknown";
  }
}

/**
 * Send a write and turn "the reply never arrived" into a fact, if the host
 * knows one.
 *
 * Every write in this file goes through here. A write that resolves means the
 * host's ledger says committed — callers say "已投递" only on that basis. A
 * write that came back `ResultUnknown` is looked up by its requestId: if the
 * ledger now says committed we return normally; if it still cannot say, the
 * original unknown error propagates and the UI reports "结果未知" rather than
 * inviting a retry that could duplicate the card.
 *
 * Rejections with a real reason (Conflict, PermissionDenied…) pass through
 * untouched — those are answers, not silence.
 */
async function requestWrite(
  host: PluginHostApi,
  method: string,
  params: Record<string, unknown>,
): Promise<void> {
  // The backend ledger keys on the request id inside the params, not on the
  // transport's own id: the transport id never reaches the backend, so a write
  // without this field could never be looked up afterwards. Generating it here
  // is what makes "the reply was lost" answerable at all.
  const requestId = newRequestId();
  try {
    await host.request(method, { ...params, request_id: requestId });
  } catch (error) {
    if (!isResultUnknown(error)) throw error;
    // Look up the id we sent, not the transport's: only ours is in the ledger.
    const settled = await settleWrite(host, requestId);
    if (settled === "committed") return;
    throw error;
  }
}

/**
 * A fresh id for one write.
 *
 * `crypto.randomUUID` is available in the webview and in jsdom; the fallback
 * keeps a test environment without it from producing the empty string, which
 * `settleWrite` treats as "nothing to look up".
 */
function newRequestId(): string {
  const maybe = (globalThis as { crypto?: { randomUUID?: () => string } }).crypto;
  if (typeof maybe?.randomUUID === "function") return maybe.randomUUID();
  return `w-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}
