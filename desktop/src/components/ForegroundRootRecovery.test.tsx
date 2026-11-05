/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, fireEvent, waitFor, cleanup } from "@testing-library/react";
import {
  ForegroundRootRecovery,
  type RecoveryOperationView,
  type RecoveryPreviewView,
} from "./ForegroundRootRecovery";

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

const impact = (overrides: Partial<Record<string, unknown>> = {}) => ({
  rootId: "root-1",
  attemptId: "attempt-1",
  stoppedExecutionIds: ["exec-1"],
  blockedExecutionIds: [],
  childIds: [],
  version: 7,
  reason: "the watchdog closed a foreground container",
  ...overrides,
});

const ready = (overrides: Partial<RecoveryPreviewView> = {}): RecoveryPreviewView => ({
  operationId: "op-1",
  token: "token-1",
  blocked: [],
  // Relative to now: a fixed date silently becomes "already expired" once the
  // wall clock passes it, which would turn every confirmable case into a refusal.
  expiresAt: new Date(Date.now() + 60 * 60 * 1000).toISOString(),
  impact: impact(),
  ...overrides,
});

/**
 * A preview the daemon withheld a token for.
 *
 * The blocked list is what makes it unconfirmable, so the fixture keeps both fields
 * consistent: a token with no blocked entity is the only confirmable shape.
 */
const blockedPreview = (blocked: string[]): RecoveryPreviewView =>
  ready({
    token: null,
    blocked,
    // The panel renders the impact's blocked list, so the fixture must carry the
    // same entities there: a preview whose two blocked lists disagreed would be a
    // wire bug, not a UI state.
    impact: impact({ blockedExecutionIds: blocked }),
  });

/**
 * Renders the panel with injected host calls.
 *
 * Every test supplies its own mocks so a passing case cannot borrow another's
 * behaviour; the component never reaches for a real Tauri command.
 */
function renderPanel({
  preview = ready(),
  confirmRecovery = vi.fn(async () => ({ phase: "committed" as const, result: committedResult() })),
  queryOperation = vi.fn(async () => null),
  cancel = vi.fn(),
}: {
  preview?: RecoveryPreviewView;
  confirmRecovery?: (operationId: string, token: string) => Promise<RecoveryOperationView>;
  queryOperation?: (operationId: string) => Promise<RecoveryOperationView | null>;
  cancel?: () => void;
} = {}) {
  return render(
    <ForegroundRootRecovery
      preview={preview}
      confirmRecovery={confirmRecovery}
      queryOperation={queryOperation}
      cancel={cancel}
    />,
  );
}

function committedResult() {
  return {
    operationId: "op-1",
    previousRootId: "root-1",
    previousAttemptId: "attempt-1",
    successorAttemptId: "attempt-2",
    bindingEpoch: 3,
    driverEpoch: 4,
  };
}

const confirmButton = () => screen.getByRole("button", { name: /确认恢复/ });
const cancelButton = () => screen.getByRole("button", { name: /取消/ });

describe("ForegroundRootRecovery", () => {
  it("shows the old attempt and refuses to confirm while an execution is unproven", () => {
    const confirmRecovery = vi.fn(async () => ({ phase: "committed" as const, result: committedResult() }));
    renderPanel({ preview: blockedPreview(["exec-2"]), confirmRecovery });

    expect(screen.getByText("attempt-1")).toBeTruthy();
    expect(screen.getByText(/exec-2/)).toBeTruthy();

    const button = confirmButton() as HTMLButtonElement;
    expect(button.disabled).toBe(true);
    fireEvent.click(button);
    expect(confirmRecovery).toHaveBeenCalledTimes(0);
  });

  it("cancels without preparing anything", () => {
    const confirmRecovery = vi.fn(async () => ({ phase: "committed" as const, result: committedResult() }));
    const cancel = vi.fn();
    renderPanel({ preview: ready(), confirmRecovery, cancel });

    fireEvent.click(cancelButton());
    expect(cancel).toHaveBeenCalledTimes(1);
    expect(confirmRecovery).toHaveBeenCalledTimes(0);
  });

  it("confirms once, names the same operation, and shows success only when committed", async () => {
    // The first query says "still ready"; confirm's answer is lost in transport; the
    // second query reports the commit. That is exactly the lost-answer path.
    const queryOperation = vi
      .fn<(id: string) => Promise<RecoveryOperationView | null>>()
      .mockResolvedValueOnce({ phase: "ready", operationId: "op-1", preview: ready() })
      .mockResolvedValueOnce({ phase: "committed", result: committedResult() });
    const confirmRecovery = vi.fn(async () => {
      throw new Error("transport");
    });

    renderPanel({ preview: ready(), confirmRecovery, queryOperation });

    fireEvent.click(confirmButton());
    fireEvent.click(confirmButton());

    await waitFor(() => expect(screen.getByText(/已完成恢复/)).toBeTruthy());
    expect(confirmRecovery).toHaveBeenCalledTimes(1);
    expect(confirmRecovery).toHaveBeenCalledWith("op-1", "token-1");
    expect(queryOperation).toHaveBeenCalledTimes(2);
    expect(queryOperation.mock.calls.every(([id]) => id === "op-1")).toBe(true);
  });

  it("never shows success for an operation that is already committed", async () => {
    const confirmRecovery = vi.fn(async () => ({ phase: "committed" as const, result: committedResult() }));
    const queryOperation = vi.fn(async () => ({
      phase: "committed" as const,
      result: committedResult(),
    }));

    renderPanel({ preview: ready(), confirmRecovery, queryOperation });

    fireEvent.click(confirmButton());
    await waitFor(() => expect(queryOperation).toHaveBeenCalledTimes(1));
    expect(confirmRecovery).toHaveBeenCalledTimes(0);
  });

  it("keeps a query failure from reading as success", async () => {
    const queryOperation = vi.fn(async () => {
      throw new Error("socket gone");
    });
    const confirmRecovery = vi.fn(async () => ({ phase: "committed" as const, result: committedResult() }));

    renderPanel({ preview: ready(), confirmRecovery, queryOperation });

    fireEvent.click(confirmButton());
    await waitFor(() => expect(screen.getByRole("alert")).toBeTruthy());
    expect(screen.queryByText(/已完成恢复/)).toBeNull();
    expect(confirmRecovery).toHaveBeenCalledTimes(0);
  });

  it("refuses an expired preview and an unparseable expiry", () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-11-04T12:02:00Z"));
    const confirmRecovery = vi.fn(async () => ({ phase: "committed" as const, result: committedResult() }));
    const stale = new Date("2026-11-04T12:01:00Z").toISOString();

    const expired = renderPanel({ preview: ready({ expiresAt: stale }), confirmRecovery });
    expect((confirmButton() as HTMLButtonElement).disabled).toBe(true);
    fireEvent.click(confirmButton());
    expect(confirmRecovery).toHaveBeenCalledTimes(0);
    expired.unmount();

    // A missing or unparseable expiry is never treated as "no deadline".
    renderPanel({ preview: ready({ expiresAt: "not-a-date" }), confirmRecovery });
    expect((confirmButton() as HTMLButtonElement).disabled).toBe(true);
    fireEvent.click(confirmButton());
    expect(confirmRecovery).toHaveBeenCalledTimes(0);
  });
});
