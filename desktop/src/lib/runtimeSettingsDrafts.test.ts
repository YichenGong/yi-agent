import { describe, expect, it } from "vitest";
import {
  acknowledgeRuntimeSave,
  dirtyKeys,
  editRuntimeDraft,
  emptyDraftState,
  mergeRuntimeRead,
  pendingPatch,
  visibleDrafts,
} from "./runtimeSettingsDrafts";
import type { RuntimeField } from "./runtimeSettings";

function field(key: string, value: string | number | null): RuntimeField {
  return {
    key,
    group: "agent",
    label: key,
    type: "number",
    value,
    default: "200",
    description: "",
    options: [],
    min: null,
    max: null,
  };
}

describe("mergeRuntimeRead", () => {
  it("adopts a remote value for a field the user has not touched", () => {
    let state = mergeRuntimeRead(emptyDraftState(), [field("agent.max_turns", 200)]);
    state = mergeRuntimeRead(state, [field("agent.max_turns", 250)]);
    expect(visibleDrafts(state)).toEqual({ "agent.max_turns": "250" });
    expect(state.conflicts).toEqual([]);
  });

  it("keeps a dirty draft when the remote value really changed", () => {
    let state = mergeRuntimeRead(emptyDraftState(), [field("agent.max_turns", 200)]);
    state = editRuntimeDraft(state, "agent.max_turns", "300");
    // Someone else saved 250 while this page held 300.
    state = mergeRuntimeRead(state, [field("agent.max_turns", 250)]);
    expect(visibleDrafts(state)["agent.max_turns"]).toBe("300");
    expect(state.conflicts).toContain("agent.max_turns");
  });

  it("confirms the draft instead of conflicting when the remote caught up", () => {
    // The user typed 300, then the save landed: the remote is now 300, which is
    // exactly the draft. Reporting a conflict here would be noise -- nothing on
    // the page is at risk.
    let state = mergeRuntimeRead(emptyDraftState(), [field("agent.max_turns", 200)]);
    state = editRuntimeDraft(state, "agent.max_turns", "300");
    state = mergeRuntimeRead(state, [field("agent.max_turns", 300)]);
    expect(visibleDrafts(state)["agent.max_turns"]).toBe("300");
    expect(state.conflicts).toEqual([]);
  });

  it("does not flag a conflict when the remote baseline is unchanged", () => {
    // The remote value moved for *other* keys only; this dirty field's baseline
    // is untouched, so there is nothing to warn about.
    let state = mergeRuntimeRead(emptyDraftState(), [
      field("agent.max_turns", 200),
      field("compact.threshold", 80),
    ]);
    state = editRuntimeDraft(state, "agent.max_turns", "300");
    state = mergeRuntimeRead(state, [
      field("agent.max_turns", 200),
      field("compact.threshold", 90),
    ]);
    expect(state.conflicts).toEqual([]);
    expect(visibleDrafts(state)["agent.max_turns"]).toBe("300");
  });
});

describe("acknowledgeRuntimeSave", () => {
  it("clears a submitted field that was not edited again while saving", () => {
    let state = mergeRuntimeRead(emptyDraftState(), [field("agent.max_turns", 200)]);
    state = editRuntimeDraft(state, "agent.max_turns", "300");
    const editsAtSubmit = { ...state.edits };
    state = acknowledgeRuntimeSave(state, { "agent.max_turns": "300" }, editsAtSubmit);
    expect(dirtyKeys(state)).toEqual([]);
    expect(pendingPatch(state)).toEqual({});
  });

  it("keeps an edit made while the save was in flight", () => {
    // 300 was submitted; the user then typed 400. The save landing must not
    // discard 400 -- that is the whole point of tracking generations.
    let state = mergeRuntimeRead(emptyDraftState(), [field("agent.max_turns", 200)]);
    state = editRuntimeDraft(state, "agent.max_turns", "300");
    const editsAtSubmit = { ...state.edits };
    state = editRuntimeDraft(state, "agent.max_turns", "400");
    state = acknowledgeRuntimeSave(state, { "agent.max_turns": "300" }, editsAtSubmit);
    expect(state.drafts["agent.max_turns"]).toBe("400");
  });

  it("leaves keys that were not part of this save alone", () => {
    let state = mergeRuntimeRead(
      emptyDraftState(),
      [field("agent.max_turns", 200), field("compact.threshold", 80)],
    );
    state = editRuntimeDraft(state, "agent.max_turns", "300");
    state = editRuntimeDraft(state, "compact.threshold", "90");
    const editsAtSubmit = { ...state.edits };
    state = acknowledgeRuntimeSave(state, { "agent.max_turns": "300" }, editsAtSubmit);
    expect(pendingPatch(state)).toEqual({ "compact.threshold": "90" });
  });

  it("clears a submitted unset (null) rather than restoring the old text", () => {
    // "Restore default" submits null. The draft must go away, not revert to the
    // previous value, otherwise the field would look dirty forever.
    let state = mergeRuntimeRead(emptyDraftState(), [field("agent.max_turns", 200)]);
    state = editRuntimeDraft(state, "agent.max_turns", "");
    const editsAtSubmit = { ...state.edits };
    state = acknowledgeRuntimeSave(state, { "agent.max_turns": null }, editsAtSubmit);
    expect(dirtyKeys(state)).toEqual([]);
  });
});
