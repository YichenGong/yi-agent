import { describe, expect, it } from "vitest";
import {
  ACQUIRE_FAILED_REASON,
  acceptsStartupReply,
  isStartupProblem,
  startupIssueReason,
} from "./runtimeStartupIssue";

describe("acceptsStartupReply", () => {
  it("rejects a reply to an older request", () => {
    expect(acceptsStartupReply(2, 1)).toBe(false);
  });

  it("accepts the reply to the request on the wire", () => {
    expect(acceptsStartupReply(2, 2)).toBe(true);
  });

  it("accepts the first reply", () => {
    expect(acceptsStartupReply(1, 1)).toBe(true);
  });
});

describe("startupIssueReason", () => {
  it("has no issue for a ready project", () => {
    expect(startupIssueReason("managedReady")).toBeNull();
    expect(isStartupProblem("managedReady")).toBe(false);
  });

  it("maps every non-ready state to fixed text", () => {
    expect(startupIssueReason("starting")).toBeTruthy();
    expect(startupIssueReason("blocked")).toBeTruthy();
    expect(startupIssueReason("unknown")).toBeTruthy();
    expect(startupIssueReason("legacyChoiceRequired")).toBeTruthy();
    // An unrecognised state is still a problem, and never rendered raw.
    expect(startupIssueReason("somethingNew")).toBe(ACQUIRE_FAILED_REASON);
    expect(isStartupProblem("somethingNew")).toBe(true);
  });

  it("never echoes a path or secret-shaped detail", () => {
    // The native reason is deliberately not an input here: the mapping exists
    // so a path or credential in that text can never reach the page.
    for (const state of ["starting", "blocked", "unknown", "legacyChoiceRequired"]) {
      const reason = startupIssueReason(state);
      expect(reason).not.toMatch(/\/|token|secret|credential/i);
    }
  });
});
