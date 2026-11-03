/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { VersionMismatchBanner } from "./VersionMismatchBanner";
import type { RuntimeStatusView } from "../lib/runtimeLifecycle";

afterEach(cleanup);

function status(over: Partial<RuntimeStatusView> = {}): RuntimeStatusView {
  const base = {
    scopeKey: "/p/a/.yi-agent/runtime",
    expectedDisplayVersion: "1.4.0",
    expectedBuildId: "build-new",
    compatibility: "compatibleMismatch",
    lifecycleManaged: true,
    localRestartAvailable: false,
    guardianRunId: null,
    daemonGeneration: 1n,
    identity: {
      daemonDisplayVersion: "1.3.0",
      daemonBuildId: "build-old",
      guardianDisplayVersion: null,
      guardianBuildId: null,
    },
    clients: [],
    activeOperation: null,
    message: null,
    snapshotId: "s",
  } as unknown as RuntimeStatusView;
  return { ...base, ...over };
}

describe("VersionMismatchBanner", () => {
  it("shows the version gap", () => {
    render(<VersionMismatchBanner status={status()} remote={false} />);
    expect(screen.getByText(/different build/)).toBeTruthy();
    expect(screen.getByText("1.3.0 → 1.4.0")).toBeTruthy();
  });

  it("renders nothing for a matching build", () => {
    const { container } = render(
      <VersionMismatchBanner status={status({ compatibility: "sameBuild" })} remote={false} />,
    );
    expect(container.firstChild).toBeNull();
  });

  it("says tasks are disabled when only control is compatible", () => {
    const onExplain = vi.fn();
    render(<VersionMismatchBanner status={status({ compatibility: "controlOnly" })} remote={false} onExplain={onExplain} />);
    expect(screen.getByText(/tasks are disabled/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /why are tasks disabled/i }));
    expect(onExplain).toHaveBeenCalledTimes(1);
  });

  it("tells a remote client to restart from the desktop", () => {
    render(<VersionMismatchBanner status={status()} remote />);
    expect(screen.getByText(/restart it from the desktop app/)).toBeTruthy();
  });

  it("does not tell a desktop user to go to the desktop", () => {
    render(<VersionMismatchBanner status={status()} remote={false} />);
    expect(screen.queryByText(/restart it from the desktop app/)).toBeNull();
  });
});
