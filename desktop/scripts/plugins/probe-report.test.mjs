// Node built-in test runner only: this file must be runnable with
// `node --test desktop/scripts/plugins/probe-report.test.mjs` and must not build
// or spawn anything. Its only import is the pure checker.
import test from "node:test";
import assert from "node:assert/strict";

import { checkProbeReport, PROBE_CASES } from "./probe-lib.mjs";

/** A report that would pass for the given case ids and statuses. */
function report(cases, extra = {}) {
  return {
    suite: "isolation",
    startedAt: "2026-11-03T00:00:00.000Z",
    finishedAt: "2026-11-03T00:00:10.000Z",
    appSha256: "a".repeat(64),
    containerConfigSha256: "b".repeat(64),
    cases,
    atomic: true,
    ...extra,
  };
}

function passing(cases = PROBE_CASES, status = "pass") {
  return cases.map((id) => ({ id, status, evidence: [`evidence for ${id}`] }));
}

test("empty report is not a pass", () => {
  assert.throws(() => checkProbeReport({ cases: [] }), /missing/);
});

test("a report missing one expected case is not a pass", () => {
  const cases = passing().filter((c) => c.id !== "native_invoke_denied");
  assert.throws(() => checkProbeReport(report(cases)), /native_invoke_denied/);
});

test("a skipped case is a failure, not a pass", () => {
  const cases = passing(PROBE_CASES, "skipped");
  assert.throws(() => checkProbeReport(report(cases)), /skipped/);
});

test("an unsupported case is a failure, not a pass", () => {
  const cases = PROBE_CASES.map((id) =>
    id === "cross_package_asset_denied"
      ? { id, status: "unsupported", evidence: ["no"] }
      : { id, status: "pass", evidence: ["ok"] },
  );
  assert.throws(
    () => checkProbeReport(report(cases)),
    /cross_package_asset_denied/,
  );
});

test("a failing case is reported with its evidence", () => {
  const cases = PROBE_CASES.map((id) =>
    id === "native_invoke_denied"
      ? { id, status: "fail", evidence: ["invoke resolved"] }
      : { id, status: "pass", evidence: ["ok"] },
  );
  assert.throws(
    () => checkProbeReport(report(cases)),
    /native_invoke_denied[\s\S]*invoke resolved/,
  );
});

test("a timestamped non-atomic report is rejected", () => {
  assert.throws(
    () => checkProbeReport(report(passing(), { atomic: false })),
    /atomic/,
  );
});

test("a missing app digest is rejected", () => {
  const r = report(passing());
  delete r.appSha256;
  assert.throws(() => checkProbeReport(r), /appSha256/);
});

test("a duplicate case entry is rejected", () => {
  const cases = passing();
  cases.push({ ...cases[0] });
  assert.throws(() => checkProbeReport(report(cases)), /duplicate/);
});

test("an unknown case id is rejected instead of ignored", () => {
  const cases = passing();
  cases.push({ id: "something_new", status: "pass", evidence: ["x"] });
  assert.throws(() => checkProbeReport(report(cases)), /something_new/);
});

test("a passing report is accepted", () => {
  const result = checkProbeReport(report(passing()));
  assert.equal(result.ok, true);
  assert.equal(result.checked, PROBE_CASES.length);
});

test("the expected case list covers every spec §4 requirement", () => {
  for (const id of [
    "dom_read_denied",
    "native_storage_read_denied",
    "native_invoke_denied",
    "cross_package_asset_denied",
    "cross_version_asset_denied",
    "navigation_and_popup_denied",
    "implicit_exfiltration_denied",
    "frame_impersonation_denied",
    "cross_frame_replay_denied",
    "blocked_script_recovery",
    "alloc_burst_is_bounded",
    "normal_interaction_works",
  ]) {
    assert.ok(PROBE_CASES.includes(id), `missing ${id}`);
  }
});

// G0 asks for evidence, not just a green check. These three cases carry the
// load-bearing claims of spec §4: a page that never recovers, a page that can
// reach native commands, and a page that can replay a handshake. If any of them
// can silently vanish from the report, "12/12 passed" means nothing.
for (const id of [
  "blocked_script_recovery",
  "native_invoke_denied",
  "cross_frame_replay_denied",
]) {
  test(`dropping \`${id}\` turns the report red, not silent`, () => {
    const cases = passing().filter((c) => c.id !== id);
    assert.throws(() => checkProbeReport(report(cases)), new RegExp(id));
  });
}

test("every load-bearing case is present in a passing report", () => {
  for (const id of [
    "blocked_script_recovery",
    "native_invoke_denied",
    "cross_frame_replay_denied",
  ]) {
    assert.ok(PROBE_CASES.includes(id), `PROBE_CASES is missing ${id}`);
  }
});
