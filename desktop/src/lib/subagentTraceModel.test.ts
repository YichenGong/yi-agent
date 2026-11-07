import { describe, it, expect } from "vitest";
import {
  cancelDeadline,
  decodeTraceRow,
  foldTraceRows,
  mergeTraceEvent,
  mergeTraceSnapshot,
  summarizeTrace,
} from "./subagentTraceModel";
import { traceRow } from "../test/subagentTraceFixtures";

describe("decodeTraceRow", () => {
  it("preserves malformed known events instead of rendering empty text", () => {
    for (const payload of [null, [], { text: 42 }, { type: "tool_call", text: "wrong tag" }]) {
      const row = traceRow(7, "assistant_text", payload);
      expect(decodeTraceRow(row)).toMatchObject({ kind: "raw", rawPayload: row.payloadJson });
    }
    const broken = { ...traceRow(8, "assistant_text", {}), payloadJson: "{broken" };
    expect(decodeTraceRow(broken).rawPayload).toBe("{broken");
  });

  it("decodes a valid assistant text row", () => {
    const row = traceRow(1, "assistant_text", { type: "assistant_text", text: "hello" });
    expect(decodeTraceRow(row)).toEqual({
      key: JSON.stringify(["task-a", 1]),
      firstEventId: 1,
      kind: "assistant_text",
      sourceKind: "assistant_text",
      text: "hello",
      name: null,
      isError: false,
      rawPayload: null,
    });
  });

  it("accepts a payload whose type tag is absent or matches the kind", () => {
    expect(decodeTraceRow(traceRow(1, "assistant_text", { text: "no tag" })).text).toBe("no tag");
    expect(
      decodeTraceRow(traceRow(2, "assistant_text", { type: "assistant_text", text: "tagged" })).text,
    ).toBe("tagged");
  });

  it("treats a wrong or non-string type tag as raw", () => {
    for (const payload of [
      { type: "tool_result", text: "wrong" },
      { type: 42, text: "wrong" },
      { type: null, text: "wrong" },
    ]) {
      const row = traceRow(3, "assistant_text", payload);
      expect(decodeTraceRow(row)).toMatchObject({ kind: "raw", rawPayload: row.payloadJson });
    }
  });

  it("treats a non-object JSON payload as raw", () => {
    for (const payload of ["hello", 42, true]) {
      const row = traceRow(4, "assistant_text", payload);
      expect(decodeTraceRow(row)).toMatchObject({ kind: "raw", rawPayload: row.payloadJson });
    }
  });

  it("decodes a tool call with its own name and summary", () => {
    const row = traceRow(5, "tool_call", { type: "tool_call", name: "bash", summary: "cargo test" });
    expect(decodeTraceRow(row)).toMatchObject({
      kind: "tool_call",
      sourceKind: "tool_call",
      name: "bash",
      text: "cargo test",
      isError: false,
      rawPayload: null,
    });
  });

  it("decodes a tool result as a self-contained block with its own name", () => {
    const block = decodeTraceRow(
      traceRow(6, "tool_result", { type: "tool_result", name: "grep", is_error: true, summary: "boom" }),
    );
    expect(block).toMatchObject({
      kind: "tool_result",
      name: "grep",
      text: "boom",
      isError: true,
      rawPayload: null,
    });
  });

  it("keeps a non-boolean is_error as raw rather than inventing false", () => {
    const row = traceRow(7, "tool_result", { name: "bash", is_error: "yes", summary: "boom" });
    expect(decodeTraceRow(row)).toMatchObject({ kind: "raw", rawPayload: row.payloadJson });
  });

  it("decodes a state note without adding display punctuation", () => {
    const block = decodeTraceRow(traceRow(8, "state_note", { type: "state_note", note: "completed" }));
    // The "· " prefix is the component's job; the model carries the note only.
    expect(block).toMatchObject({ kind: "state_note", text: "completed", name: null });
  });

  it("treats unknown kinds as raw with the full original payload", () => {
    const row = traceRow(9, "future_kind", { anything: "here" }, "task-b");
    const block = decodeTraceRow(row);
    expect(block).toMatchObject({ kind: "raw", sourceKind: "future_kind", rawPayload: row.payloadJson });
    expect(block.key).toBe(JSON.stringify(["task-b", 9]));
    expect(block.text).not.toBe("");
  });

  it("keys a raw block by task and event, not by array position", () => {
    const first = traceRow(3, "future_kind", { a: 1 }, "task-a");
    const second = traceRow(3, "future_kind", { a: 2 }, "task-b");
    expect(decodeTraceRow(first).key).toBe(JSON.stringify(["task-a", 3]));
    expect(decodeTraceRow(second).key).toBe(JSON.stringify(["task-b", 3]));
    expect(decodeTraceRow(first).key).not.toBe(decodeTraceRow(second).key);
  });
});

describe("foldTraceRows", () => {
  it("merges two adjacent assistant text blocks and keeps the first block's key", () => {
    const first = traceRow(1, "assistant_text", { text: "one " });
    const second = traceRow(2, "assistant_text", { text: "two" });
    const blocks = foldTraceRows([first, second]);
    expect(blocks).toHaveLength(1);
    expect(blocks[0].key).toBe(decodeTraceRow(first).key);
    expect(blocks[0].firstEventId).toBe(1);
    expect(blocks[0].text).toBe("one two");
  });

  it("does not merge assistant text across a tool call or result", () => {
    const blocks = foldTraceRows([
      traceRow(1, "assistant_text", { text: "one" }),
      traceRow(2, "tool_call", { name: "bash", summary: "ls" }),
      traceRow(3, "tool_result", { name: "bash", is_error: false, summary: "ok" }),
      traceRow(4, "assistant_text", { text: "after" }),
    ]);
    expect(blocks.map((b) => b.kind)).toEqual([
      "assistant_text",
      "tool_call",
      "tool_result",
      "assistant_text",
    ]);
    expect(blocks.map((b) => b.text)).toEqual(["one", "ls", "ok", "after"]);
    expect(blocks[1].name).toBe("bash");
    expect(blocks[2].name).toBe("bash");
  });

  it("does not merge assistant text across a malformed known row", () => {
    const broken = traceRow(2, "assistant_text", { text: 42 });
    const blocks = foldTraceRows([
      traceRow(1, "assistant_text", { text: "one" }),
      broken,
      traceRow(3, "assistant_text", { text: "two" }),
    ]);
    expect(blocks.map((b) => b.kind)).toEqual(["assistant_text", "raw", "assistant_text"]);
    expect(blocks[1].rawPayload).toBe(broken.payloadJson);
  });

  it("does not merge assistant text across an unknown kind, which stays raw", () => {
    const unknown = traceRow(2, "future_kind", { x: 1 });
    const blocks = foldTraceRows([
      traceRow(1, "assistant_text", { text: "one" }),
      unknown,
      traceRow(3, "assistant_text", { text: "two" }),
    ]);
    expect(blocks).toHaveLength(3);
    expect(blocks[1]).toMatchObject({ kind: "raw", rawPayload: unknown.payloadJson });
  });

  it("merges a run of assistant text into pairwise adjacent blocks", () => {
    const blocks = foldTraceRows([
      traceRow(1, "assistant_text", { text: "a" }),
      traceRow(2, "assistant_text", { text: "b" }),
      traceRow(3, "assistant_text", { text: "c" }),
    ]);
    expect(blocks).toHaveLength(1);
    expect(blocks[0].text).toBe("abc");
  });

  it("returns no blocks for no rows", () => {
    expect(foldTraceRows([])).toEqual([]);
  });
});

describe("mergeTraceSnapshot", () => {
  it("uses snapshot precedence and only post-watermark buffered events", () => {
    const old = traceRow(10, "state_note", { note: "old" });
    const authoritative = traceRow(10, "state_note", { note: "snapshot" });
    const next = traceRow(24, "assistant_text", { text: "new" });
    const rows = mergeTraceSnapshot("task-a", { rows: [authoritative], highWaterId: 10 }, [old, next, next]);
    expect(rows.map((r) => r.eventId)).toEqual([10, 24]);
    expect(rows[0]).toEqual(authoritative);
  });

  it("dedupes and numerically sorts the snapshot's own rows", () => {
    const rows = mergeTraceSnapshot(
      "task-a",
      {
        rows: [
          traceRow(10, "assistant_text", { text: "ten" }),
          traceRow(9, "assistant_text", { text: "nine" }),
          traceRow(10, "assistant_text", { text: "ten again" }),
        ],
        highWaterId: 10,
      },
      [],
    );
    expect(rows.map((r) => r.eventId)).toEqual([9, 10]);
    expect(rows[1].payloadJson).toBe(traceRow(10, "assistant_text", { text: "ten" }).payloadJson);
  });

  it("filters both snapshot and buffered rows by task first", () => {
    const rows = mergeTraceSnapshot(
      "task-a",
      { rows: [traceRow(1, "assistant_text", { text: "mine" })], highWaterId: 1 },
      [
        traceRow(2, "assistant_text", { text: "buffered mine" }),
        traceRow(3, "assistant_text", { text: "someone else" }, "task-b"),
      ],
    );
    expect(rows.map((r) => r.taskId)).toEqual(["task-a", "task-a"]);
    expect(rows.map((r) => r.eventId)).toEqual([1, 2]);
  });

  it("accepts every buffered row above a zero watermark", () => {
    const rows = mergeTraceSnapshot("task-a", { rows: [], highWaterId: 0 }, [
      traceRow(2, "assistant_text", { text: "b" }),
      traceRow(1, "assistant_text", { text: "a" }),
    ]);
    expect(rows.map((r) => r.eventId)).toEqual([1, 2]);
  });

  it("drops buffered rows at or below the watermark", () => {
    const rows = mergeTraceSnapshot("task-a", { rows: [], highWaterId: 5 }, [
      traceRow(5, "assistant_text", { text: "at" }),
      traceRow(4, "assistant_text", { text: "below" }),
      traceRow(6, "assistant_text", { text: "above" }),
    ]);
    expect(rows.map((r) => r.eventId)).toEqual([6]);
  });

  it("sorts numerically, not lexicographically", () => {
    const rows = mergeTraceSnapshot("task-a", { rows: [], highWaterId: 0 }, [
      traceRow(10, "assistant_text", { text: "ten" }),
      traceRow(9, "assistant_text", { text: "nine" }),
    ]);
    expect(rows.map((r) => r.eventId)).toEqual([9, 10]);
  });
});

describe("mergeTraceEvent", () => {
  it("returns the same rows reference when the event id already exists", () => {
    const rows = [traceRow(1, "assistant_text", { text: "one" })];
    expect(mergeTraceEvent("task-a", rows, rows[0])).toBe(rows);
  });

  it("does not re-concatenate an existing event's text", () => {
    const rows = [traceRow(1, "assistant_text", { text: "one" })];
    const merged = mergeTraceEvent("task-a", rows, traceRow(1, "assistant_text", { text: "one" }));
    expect(merged).toBe(rows);
    expect(foldTraceRows(merged).map((b) => b.text)).toEqual(["one"]);
  });

  it("appends a new event in numeric order", () => {
    const rows = [traceRow(1, "assistant_text", { text: "one" }), traceRow(10, "assistant_text", { text: "ten" })];
    const merged = mergeTraceEvent("task-a", rows, traceRow(9, "assistant_text", { text: "nine" }));
    expect(merged.map((r) => r.eventId)).toEqual([1, 9, 10]);
    expect(rows.map((r) => r.eventId)).toEqual([1, 10]);
  });

  it("ignores an event that belongs to another task", () => {
    const rows = [traceRow(1, "assistant_text", { text: "one" })];
    expect(mergeTraceEvent("task-a", rows, traceRow(2, "assistant_text", { text: "theirs" }, "task-b"))).toBe(
      rows,
    );
  });
});

describe("summarizeTrace", () => {
  it("counts Unicode code points and explicit logical lines", () => {
    expect(summarizeTrace("😀".repeat(240)).folded).toBe(false);
    expect(summarizeTrace("😀".repeat(241)).preview).toBe("😀".repeat(240) + "…");
    expect(summarizeTrace("a\nb\nc").folded).toBe(false);
    expect(summarizeTrace("a\nb\nc\nd")).toMatchObject({ folded: true, preview: "a…" });
  });

  it("does not fold exactly three logical lines or 240 code points", () => {
    expect(summarizeTrace("a\nb\nc")).toMatchObject({
      folded: false,
      preview: "a\nb\nc",
      full: "a\nb\nc",
    });
  });

  it("folds a single line longer than 240 code points", () => {
    const preview = summarizeTrace("x".repeat(241));
    expect(preview).toMatchObject({ folded: true, preview: "x".repeat(240) + "…" });
    expect(preview.full).toBe("x".repeat(241));
  });

  it("splits logical lines on CRLF and bare CR", () => {
    expect(summarizeTrace("a\r\nb\r\nc").folded).toBe(false);
    expect(summarizeTrace("a\r\nb\r\nc\r\nd")).toMatchObject({ folded: true, preview: "a…" });
    expect(summarizeTrace("a\rb\rc\rd")).toMatchObject({ folded: true, preview: "a…" });
  });

  it("handles empty text and an empty first line", () => {
    expect(summarizeTrace("")).toMatchObject({ folded: false, preview: "", full: "" });
    expect(summarizeTrace("\na\nb\nc")).toMatchObject({ folded: true, preview: "…", full: "\na\nb\nc" });
  });
});

describe("cancelDeadline", () => {
  it("adds the TTL in milliseconds", () => {
    expect(cancelDeadline(1000, 30)).toBe(31_000);
    expect(cancelDeadline(1000, 0.5)).toBe(1500);
  });

  it("returns null for a non-positive TTL", () => {
    expect(cancelDeadline(1000, 0)).toBeNull();
    expect(cancelDeadline(1000, -1)).toBeNull();
  });

  it("returns null for non-finite inputs", () => {
    expect(cancelDeadline(Number.NaN, 30)).toBeNull();
    expect(cancelDeadline(Number.POSITIVE_INFINITY, 30)).toBeNull();
    expect(cancelDeadline(1000, Number.NaN)).toBeNull();
    expect(cancelDeadline(1000, Number.POSITIVE_INFINITY)).toBeNull();
  });

  it("returns null when the addition overflows to a non-finite deadline", () => {
    // 1e308 + 1e305 * 1000 = 2e308, i.e. Infinity.
    expect(cancelDeadline(1e308, 1e305)).toBeNull();
  });

  it("returns an already-past deadline as a number rather than hiding it", () => {
    expect(cancelDeadline(1, 1)).toBe(1001);
  });
});
