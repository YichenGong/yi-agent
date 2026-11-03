/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { RuntimeVersionBanner, RuntimeVersionBannerView } from "./RuntimeVersionBanner";
import type { RuntimeControl } from "../lib/runtimeLifecycle";
import type { LifecycleState, StatusSummary } from "../lib/runtimeLifecycle";
import type { RestartPreview, RuntimeStatusView } from "../lib/runtimeLifecycle";

afterEach(cleanup);

function status(over: Partial<RuntimeStatusView> = {}): RuntimeStatusView {
  const base = {
    scopeKey: "/p/a/.yi-agent/runtime",
    expectedDisplayVersion: "1.4.0",
    expectedBuildId: "build-new",
    compatibility: "compatibleMismatch",
    lifecycleManaged: true,
    localRestartAvailable: true,
    guardianRunId: "guardian-1",
    daemonGeneration: 3n,
    identity: {
      scopeKey: "/p/a/.yi-agent/runtime",
      daemonInstanceId: "daemon-1",
      guardianInstanceId: "guardian-1",
      daemonDisplayVersion: "1.3.0",
      daemonBuildId: "build-old",
      guardianDisplayVersion: "1.3.0",
      guardianBuildId: "build-old",
      mode: "managed",
      businessProtocol: "1",
      controlProtocol: "1",
    },
    clients: [],
    activeOperation: null,
    message: null,
    snapshotId: "snap-1",
  } as unknown as RuntimeStatusView;
  return { ...base, ...over };
}

function preview(over: Partial<RestartPreview> = {}): RestartPreview {
  return {
    token: "token-1",
    snapshotId: "snap-1",
    counts: {
      clients: 1n,
      pendingClients: 0n,
      running: 0n,
      queued: 0n,
      plugins: 0n,
      complete: true,
    },
    requiresSecondConfirmation: false,
    canRestart: true,
    blockedReason: null,
    entries: [],
    revisions: [],
    ...over,
  } as unknown as RestartPreview;
}

function state(over: Partial<LifecycleState> = {}): LifecycleState {
  return {
    status: status(),
    preview: null,
    operation: null,
    phase: "idle",
    error: null,
    operationId: null,
    busy: false,
    controlUnavailable: false,
    ...over,
  };
}

const handlers = () => ({
  onRestart: vi.fn(),
  onConfirm: vi.fn(),
  onCancel: vi.fn(),
  onRefresh: vi.fn(),
});

describe("RuntimeVersionBanner", () => {
  it("shows current and expected versions with a restart action", () => {
    const props = handlers();
    render(<RuntimeVersionBannerView state={state()} {...props} />);
    expect(screen.getByText(/1\.3\.0/)).toBeTruthy();
    expect(screen.getByText(/1\.4\.0/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /restart runtime/i }));
    expect(props.onRestart).toHaveBeenCalledTimes(1);
  });

  it("renders nothing when the builds match", () => {
    const { container } = render(
      <RuntimeVersionBannerView state={state({ status: status({ compatibility: "sameBuild" }) })} {...handlers()} />,
    );
    expect(container.firstChild).toBeNull();
  });

  it("says the exit guarantee is not in effect for a legacy runtime and offers no restart", () => {
    render(
      <RuntimeVersionBannerView
        state={state({
          status: status({ compatibility: "legacyTemporaryChoiceRequired", lifecycleManaged: false }),
        })}
        {...handlers()}
      />,
    );
    expect(screen.getByText(/not in effect/)).toBeTruthy();
    expect(screen.queryByRole("button", { name: /restart runtime/i })).toBeNull();
    expect(screen.getByText(/not started by/)).toBeTruthy();
  });

  it("explains that a runtime without the control surface must be updated", () => {
    render(<RuntimeVersionBannerView state={state({ controlUnavailable: true })} {...handlers()} />);
    expect(screen.getByText(/update both the app and the runtime/)).toBeTruthy();
    expect(screen.queryByRole("button", { name: /restart runtime/i })).toBeNull();
  });

  it("lists what a restart would interrupt before confirming", () => {
    const props = handlers();
    render(
      <RuntimeVersionBannerView
        state={state({
          preview: preview({
            counts: {
              clients: 2n,
              pendingClients: 0n,
              running: 1n,
              queued: 0n,
              plugins: 0n,
              complete: true,
            },
            entries: [
              { kind: "client", id: "tui-1", owner: "gong" },
              { kind: "execution", id: "run-9", owner: null },
            ],
          }),
          phase: "awaiting-confirmation",
        })}
        {...props}
      />,
    );
    expect(screen.getByRole("dialog", { name: /restart the background runtime/i })).toBeTruthy();
    expect(screen.getByText(/1 task\(s\) are running/)).toBeTruthy();
    expect(screen.getByText(/tui-1/)).toBeTruthy();
    expect(screen.getByText(/interrupt other clients/)).toBeTruthy();
    // A second confirmation exists precisely so the button says so.
    fireEvent.click(screen.getByRole("button", { name: /restart anyway/i }));
    expect(props.onConfirm).toHaveBeenCalledTimes(1);
  });

  it("refuses to claim nothing is running when the count is incomplete", () => {
    render(
      <RuntimeVersionBannerView
        state={state({
          preview: preview({
            counts: {
              clients: 1n,
              pendingClients: 0n,
              running: 0n,
              queued: 0n,
              plugins: 0n,
              complete: false,
            },
          }),
          phase: "awaiting-confirmation",
        })}
        {...handlers()}
      />,
    );
    expect(screen.getByText(/assume some are/)).toBeTruthy();
    expect(screen.queryByText(/No task is running/)).toBeNull();
  });

  it("cancelling the dialog calls only the local cancel", () => {
    const props = handlers();
    render(
      <RuntimeVersionBannerView state={state({ preview: preview(), phase: "awaiting-confirmation" })} {...props} />,
    );
    fireEvent.click(screen.getByRole("button", { name: /^cancel$/i }));
    expect(props.onCancel).toHaveBeenCalledTimes(1);
    expect(props.onConfirm).not.toHaveBeenCalled();
  });

  it("reports a switch in progress without offering the action again", () => {
    render(<RuntimeVersionBannerView state={state({ phase: "switching" })} {...handlers()} />);
    expect(screen.getByText(/restarting the background runtime/i)).toBeTruthy();
    expect(screen.queryByRole("button", { name: /restart runtime/i })).toBeNull();
  });

  it("asks the user to refresh when the outcome is unknown", () => {
    const props = handlers();
    render(
      <RuntimeVersionBannerView
        state={state({ phase: "unknown", error: "the runtime no longer knows this operation" })}
        {...props}
      />,
    );
    expect(screen.getByText(/no longer knows/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /refresh/i }));
    expect(props.onRefresh).toHaveBeenCalledTimes(1);
  });
});

/**
 * A complete `RuntimeControl`, built from the same DTO fixtures above.
 *
 * Every method is a `vi.fn` so a test can assert on it. The DTOs come from the
 * shared fixtures rather than `{} as ...`: a fake that returned an empty object
 * would pass type checking while telling the component nothing real, which is
 * exactly how a banner test ends up asserting on a blank screen.
 */
function makeControl(over: {
  otherClients?: bigint;
  running?: bigint;
  confirm?: () => Promise<never>;
} = {}) {
  const otherClients = over.otherClients ?? 0n;
  const running = over.running ?? 0n;
  const control = {
    status: vi.fn(async () => status({ clients: [] })),
    preview: vi.fn(async () =>
      preview({
        counts: {
          clients: otherClients + 1n,
          pendingClients: 0n,
          running,
          queued: 0n,
          plugins: 0n,
          complete: true,
        },
        entries: Array.from({ length: Number(otherClients) }, (_unused, index) => ({
          kind: "client",
          id: `client-${index}`,
          owner: null,
        })),
      }),
    ),
    confirm: vi.fn(
      over.confirm ??
        (async () => ({
          operationId: "op-1",
          phase: "switching",
          detail: null,
          readyIdentity: null,
        })),
    ),
    operation: vi.fn(async () => ({
      operationId: "op-1",
      phase: "switching",
      detail: null,
      readyIdentity: null,
    })),
  };
  return control as unknown as RuntimeControl & typeof control;
}

describe("RuntimeVersionBanner (container)", () => {
  it("does not confirm when the user cancels the impact dialog", async () => {
    const control = makeControl({ otherClients: 2n, running: 3n });
    render(
      <RuntimeVersionBanner
        scopeKey="project-a"
        control={control as unknown as RuntimeControl}
      />,
    );
    fireEvent.click(await screen.findByRole("button", { name: /restart runtime/i }));
    fireEvent.click(await screen.findByRole("button", { name: /^cancel$/i }));
    expect(control.confirm).not.toHaveBeenCalled();
  });

  it("confirms exactly once, with the preview token and a chosen operation id", async () => {
    const control = makeControl({ otherClients: 1n });
    render(
      <RuntimeVersionBanner
        scopeKey="project-a"
        control={control as unknown as RuntimeControl}
      />,
    );
    fireEvent.click(await screen.findByRole("button", { name: /restart runtime/i }));
    const confirmButton = await screen.findByRole("button", { name: /restart anyway/i });
    fireEvent.click(confirmButton);
    fireEvent.click(confirmButton);
    expect(control.confirm).toHaveBeenCalledTimes(1);
    const [, token, operationId] = control.confirm.mock.calls[0] as unknown as [
      string,
      string,
      string,
    ];
    expect(token).toBe("token-1");
    expect(operationId).toEqual(expect.any(String));
    expect(operationId.length).toBeGreaterThan(0);
  });

  it("asks the shell for the scope's remembered operation instead of inventing one", async () => {
    const control = makeControl();
    const readOperationId = vi.fn(async () => "op-remembered");
    render(
      <RuntimeVersionBanner
        scopeKey="project-a"
        control={control as unknown as RuntimeControl}
        readOperationId={readOperationId}
      />,
    );
    await vi.waitFor(() => expect(readOperationId).toHaveBeenCalledWith("project-a"));
    await vi.waitFor(() => expect(control.operation).toHaveBeenCalledWith("project-a", "op-remembered"));
  });
});

/** Kept so an unused import of the derived type does not go unnoticed. */
export type { StatusSummary };
