/**
 * Pure model for one subagent's trace: strict decoding, folding, merge, summary.
 *
 * Kept free of React and of the RPC client so the rules that decide "what the
 * user actually sees" are testable as plain data transformations.
 *
 * The governing rule is **no data loss**: a payload that does not match its
 * kind's schema — wrong field types, a mismatched `type` tag, broken JSON, or a
 * kind this client does not know — is preserved verbatim as a `raw` block.
 * Rendering an empty line for a row the daemon did send would hide the agent's
 * actual output, which is exactly what a trace exists to show.
 */
import type { AgentTraceRow, AgentTraceSnapshotResult } from "./protocol";

export interface TraceBlock {
  /** taskId + first eventId; never an array index, which shifts as rows arrive. */
  key: string;
  firstEventId: number;
  kind: "assistant_text" | "tool_call" | "tool_result" | "state_note" | "raw";
  /** The row's own kind, preserved so a raw block can still say what it was. */
  sourceKind: string;
  /** The decoded body, or the original payload for a raw block. */
  text: string;
  /** Tool name for `tool_call` / `tool_result`; null otherwise. */
  name: string | null;
  isError: boolean;
  /** The original payload for a raw block; null for every decoded block. */
  rawPayload: string | null;
}

export interface SummaryPreview {
  folded: boolean;
  /** The first logical line, clipped to 240 code points, plus "…" when folded. */
  preview: string;
  /** The whole text, so a caller never needs a second copy of the input. */
  full: string;
}

/** A rendered preview shows at most this many code points of the first line. */
const PREVIEW_CODE_POINTS = 240;

/** More logical lines than this, or more code points, is folded. */
const PREVIEW_LINES = 3;

const ELLIPSIS = "…";

/** The JSON value of a payload: an object's own keys, or null if it is anything else. */
type JsonObject = Record<string, unknown>;

/**
 * Parse a row's payload as a JSON object.
 *
 * Arrays, primitives and broken JSON are all "not an object" here: a known kind
 * whose payload has the wrong top-level shape is as malformed as a wrong field.
 */
function parseObject(payloadJson: string): JsonObject | null {
  let parsed: unknown;
  try {
    parsed = JSON.parse(payloadJson);
  } catch {
    return null;
  }
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) return null;
  return parsed as JsonObject;
}

/** A required string field, or null when absent or of another type. */
function stringField(payload: JsonObject, field: string): string | null {
  const value = payload[field];
  return typeof value === "string" ? value : null;
}

/**
 * Whether the payload's internal tag agrees with the row's kind.
 *
 * The tag is optional (older writers may omit it) but, when present, must name
 * the same kind: a `tool_call` payload delivered under an `assistant_text` kind
 * means the two columns disagree, and guessing which one is right would show
 * the user a line that belongs to neither event.
 */
function tagMatchesKind(payload: JsonObject, kind: string): boolean {
  const tag = payload["type"];
  return tag === undefined || tag === kind;
}

/** One row preserved verbatim: the payload is the display text, never empty. */
function rawBlock(row: AgentTraceRow): TraceBlock {
  return {
    key: JSON.stringify([row.taskId, row.eventId]),
    firstEventId: row.eventId,
    kind: "raw",
    sourceKind: row.kind,
    text: row.payloadJson,
    name: null,
    isError: false,
    rawPayload: row.payloadJson,
  };
}

/**
 * Decode one row into a display block, or preserve it when it does not fit.
 *
 * The required fields per kind are the whole schema; extra fields are ignored
 * (the daemon may add metadata), but a field of the wrong type is not coerced.
 * A `false` invented for a missing `is_error` would render a failed tool as a
 * success, so every required check fails closed to `raw`.
 */
export function decodeTraceRow(row: AgentTraceRow): TraceBlock {
  const payload = parseObject(row.payloadJson);
  if (payload === null || !tagMatchesKind(payload, row.kind)) return rawBlock(row);

  const base = {
    key: JSON.stringify([row.taskId, row.eventId]),
    firstEventId: row.eventId,
    sourceKind: row.kind,
    name: null as string | null,
    isError: false,
    rawPayload: null as string | null,
  };

  switch (row.kind) {
    case "assistant_text": {
      const text = stringField(payload, "text");
      if (text === null) return rawBlock(row);
      return { ...base, kind: "assistant_text", text };
    }
    case "tool_call": {
      const name = stringField(payload, "name");
      const summary = stringField(payload, "summary");
      if (name === null || summary === null) return rawBlock(row);
      return { ...base, kind: "tool_call", name, text: summary };
    }
    case "tool_result": {
      const name = stringField(payload, "name");
      const summary = stringField(payload, "summary");
      const isError = payload["is_error"];
      if (name === null || summary === null || typeof isError !== "boolean") return rawBlock(row);
      return { ...base, kind: "tool_result", name, text: summary, isError };
    }
    case "state_note": {
      const note = stringField(payload, "note");
      if (note === null) return rawBlock(row);
      // No display prefix here: punctuation is the component's concern, so the
      // model stays plain text that later summaries/composers can reuse.
      return { ...base, kind: "state_note", text: note };
    }
    default:
      return rawBlock(row);
  }
}

/**
 * Fold rows into blocks, merging runs of adjacent assistant text.
 *
 * The writer flushes text every 512 bytes or 200ms, so consecutive
 * `assistant_text` rows are one message split by a flush threshold rather than
 * two messages. Only *adjacent* text merges: a tool call, result, state note or
 * a raw block between two texts is a real boundary, and the merged block keeps
 * the first row's key so React sees one stable identity for the whole run.
 *
 * The fold is a pure function of the whole list: a client appending to a mutable
 * block would have to remember where the previous frame ended, while folding
 * what it already holds cannot drift.
 */
export function foldTraceRows(rows: readonly AgentTraceRow[]): TraceBlock[] {
  const blocks: TraceBlock[] = [];
  for (const row of rows) {
    const block = decodeTraceRow(row);
    const last = blocks[blocks.length - 1];
    if (block.kind === "assistant_text" && last?.kind === "assistant_text") {
      blocks[blocks.length - 1] = { ...last, text: last.text + block.text };
      continue;
    }
    blocks.push(block);
  }
  return blocks;
}

/**
 * Combine an authoritative snapshot with events buffered while it was in flight.
 *
 * The snapshot wins on any shared id: it is the server's whole current list,
 * whereas a buffered event may predate it. Buffered rows are therefore only
 * accepted strictly above the snapshot's watermark — `eventId === highWaterId`
 * is already represented by the snapshot, and accepting it would duplicate the
 * row and, for text, repeat its delta.
 *
 * Both inputs are filtered by `taskId` first (a shared buffer holds other
 * tasks' events), then deduped by id and sorted numerically so the fold sees
 * the daemon's order even when a duplicate or an out-of-order batch arrives.
 */
export function mergeTraceSnapshot(
  taskId: string,
  snapshot: AgentTraceSnapshotResult,
  buffered: readonly AgentTraceRow[],
): AgentTraceRow[] {
  const byId = new Map<number, AgentTraceRow>();
  for (const row of snapshot.rows) {
    if (row.taskId !== taskId) continue;
    // First occurrence wins: duplicates collide on one key and the earliest is
    // the one the list was built from; a later copy cannot add information.
    if (!byId.has(row.eventId)) byId.set(row.eventId, row);
  }
  for (const row of buffered) {
    if (row.taskId !== taskId) continue;
    if (row.eventId <= snapshot.highWaterId) continue;
    // The snapshot is authoritative for ids it already contains.
    if (byId.has(row.eventId)) continue;
    byId.set(row.eventId, row);
  }
  return [...byId.values()].sort((a, b) => a.eventId - b.eventId);
}

/**
 * Insert one live event into the rows already shown.
 *
 * An id that is already present returns the *same* array: re-appending a row
 * the snapshot or an earlier frame already carries would concatenate its text
 * a second time, and returning the identical reference stops every downstream
 * memo from re-rendering for a duplicate. Events for another task are ignored
 * here because the caller's view belongs to one task only.
 */
export function mergeTraceEvent(
  taskId: string,
  rows: readonly AgentTraceRow[],
  event: AgentTraceRow,
): readonly AgentTraceRow[] {
  if (event.taskId !== taskId) return rows;
  if (rows.some((row) => row.eventId === event.eventId)) return rows;
  return [...rows, event].sort((a, b) => a.eventId - b.eventId);
}

/**
 * Summarize trace text for the collapsed view.
 *
 * Counting is by Unicode code point (`Array.from`), not UTF-16 unit: a 240-emoji
 * line is 480 code units but 240 characters, and clipping by code units would
 * split a surrogate pair into garbage. Lines are split explicitly on CRLF, CR
 * and LF so a message written on any platform folds the same way.
 *
 * A folded summary keeps the first logical line only — the paragraph's opening
 * is the part that tells the reader whether to expand — clipped to 240 code
 * points with an ellipsis. Unfolded text is returned whole, including its
 * newlines, so the caller can render it without a second copy.
 */
export function summarizeTrace(text: string): SummaryPreview {
  const lines = text.split(/\r\n|\r|\n/);
  const folded = lines.length > PREVIEW_LINES || Array.from(text).length > PREVIEW_CODE_POINTS;
  if (!folded) return { folded: false, preview: text, full: text };
  const firstLine = Array.from(lines[0]).slice(0, PREVIEW_CODE_POINTS).join("");
  return { folded: true, preview: firstLine + ELLIPSIS, full: text };
}

/**
 * When a cancel confirmation stops being valid, in epoch milliseconds.
 *
 * A non-finite or non-positive TTL means the server did not grant a usable
 * window; returning `null` makes that explicit instead of handing back `NaN` or
 * an already-past timestamp that would render a countdown no one can act on.
 */
export function cancelDeadline(startedAt: number, ttlSeconds: number): number | null {
  if (!Number.isFinite(startedAt) || !Number.isFinite(ttlSeconds) || ttlSeconds <= 0) return null;
  const deadline = startedAt + ttlSeconds * 1000;
  return Number.isFinite(deadline) ? deadline : null;
}
