// Pure validation for the isolated-plugin-WebView probe report.
//
// This module is imported by the probe runner and by its tests. It must stay
// side-effect free: no process spawn, no filesystem writes, no network, no
// build steps. Everything here is a function over a plain parsed JSON object.
//
// A probe report is evidence, so the rules are deliberately unforgiving: a
// report is only a pass when *every* expected case ran and passed with its own
// evidence. `skipped` and `unsupported` are failures — the design spec
// (docs/superpowers/specs/2026-11-02-kanban-independent-release-design.md §4)
// does not allow reducing a requirement to "this platform does not support it".

/**
 * The cases a complete isolation-suite report must contain, one per §4
 * requirement. Order matters only for report readability; the checker compares
 * sets.
 */
export const PROBE_CASES = Object.freeze([
  // §4.1 — page cannot read the host DOM or host storage.
  "dom_read_denied",
  "native_storage_read_denied",
  // §4.2 — page cannot call Tauri invoke / native opener / dialogs.
  "native_invoke_denied",
  // §4.3 — resources only from the current package's whitelist.
  "cross_package_asset_denied",
  "cross_version_asset_denied",
  // §4.4 — no self-networking, top-level navigation, popups, child frames or
  // implicit exfiltration through scripts/styles/images.
  "navigation_and_popup_denied",
  "implicit_exfiltration_denied",
  // §4.6/§4.7 — a frame cannot claim another frame's identity or replay an
  // initialization handshake, and a runaway/blocked frame is recoverable from
  // the native side without losing persisted session state.
  "frame_impersonation_denied",
  "cross_frame_replay_denied",
  "blocked_script_recovery",
  "alloc_burst_is_bounded",
  // §4.6 — the normal path still works: IME, keyboard/focus, scrolling,
  // theme tokens and panel sizing.
  "normal_interaction_works",
]);

const PASSING_STATUS = "pass";
const SHA256 = /^[0-9a-f]{64}$/;

function fail(message) {
  throw new Error(message);
}

function requireString(value, field) {
  if (typeof value !== "string" || value.trim() === "") {
    fail(`report is missing a non-empty string \`${field}\``);
  }
}

function requireDigest(report, field) {
  const value = report[field];
  if (value === undefined || value === null) {
    fail(`report is missing \`${field}\``);
  }
  if (typeof value !== "string" || !SHA256.test(value)) {
    fail(`report \`${field}\` must be a lowercase hex sha256 digest`);
  }
}

/**
 * Validate one probe report.
 *
 * Returns `{ ok: true, checked, suite }` on success and throws an `Error`
 * describing the first reason the report is not acceptable evidence.
 *
 * @param {unknown} report parsed JSON of the report file
 */
export function checkProbeReport(report) {
  if (report === null || typeof report !== "object" || Array.isArray(report)) {
    fail("probe report is missing: expected a JSON object");
  }

  requireString(report.suite, "suite");
  requireString(report.startedAt, "startedAt");
  requireString(report.finishedAt, "finishedAt");
  requireDigest(report, "appSha256");
  requireDigest(report, "containerConfigSha256");

  // The native side writes the report atomically (temp file + rename); a
  // non-atomic report may be a partial read and cannot be trusted as evidence.
  if (report.atomic !== true) {
    fail("report is not marked atomic: a partially written report is not evidence");
  }

  if (!Array.isArray(report.cases)) {
    fail("probe report is missing the `cases` array");
  }
  if (report.cases.length === 0) {
    fail("probe report is missing every expected case: `cases` is empty");
  }

  const expected = new Set(PROBE_CASES);
  const seen = new Set();
  const failures = [];

  for (const entry of report.cases) {
    if (entry === null || typeof entry !== "object") {
      fail("probe report contains a case that is not an object");
    }
    const { id, status, evidence } = entry;
    requireString(id, "cases[].id");

    if (!expected.has(id)) {
      fail(`probe report contains unknown case \`${id}\``);
    }
    if (seen.has(id)) {
      fail(`probe report contains a duplicate entry for case \`${id}\``);
    }
    seen.add(id);

    if (status === "skipped") {
      failures.push(`${id}: skipped — a skipped requirement is a failure`);
      continue;
    }
    if (status === "unsupported") {
      failures.push(
        `${id}: unsupported — an unsupported requirement is a failure, not a reduced scope`,
      );
      continue;
    }
    if (status !== PASSING_STATUS) {
      const details = Array.isArray(evidence) ? evidence.join("; ") : String(evidence ?? "");
      failures.push(`${id}: ${status}${details ? ` — ${details}` : ""}`);
    }
  }

  for (const id of PROBE_CASES) {
    if (!seen.has(id)) {
      fail(`probe report is missing the expected case \`${id}\``);
    }
  }

  if (failures.length > 0) {
    fail(`probe report has failing cases:\n${failures.join("\n")}`);
  }

  return { ok: true, checked: PROBE_CASES.length, suite: report.suite };
}
