/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, cleanup, act } from "@testing-library/react";
import { useState, type ReactElement } from "react";
import { ErrorBoundary } from "./ErrorBoundary";

afterEach(cleanup);

function Boom(): ReactElement {
  throw new Error("boom");
}

describe("ErrorBoundary", () => {
  it("keeps the pre-resetKey behaviour when resetKey is omitted", () => {
    const spy = vi.spyOn(console, "error").mockImplementation(() => {});
    render(
      <ErrorBoundary fallback={<div data-testid="fb">failed</div>}>
        <Boom />
      </ErrorBoundary>,
    );
    expect(screen.getByTestId("fb")).toBeTruthy();
    spy.mockRestore();
  });

  it("calls onError exactly once", () => {
    const spy = vi.spyOn(console, "error").mockImplementation(() => {});
    const onError = vi.fn();
    render(
      <ErrorBoundary onError={onError} fallback={<div data-testid="fb">failed</div>}>
        <Boom />
      </ErrorBoundary>,
    );
    expect(onError).toHaveBeenCalledTimes(1);
    spy.mockRestore();
  });

  it("renders children untouched when nothing throws", () => {
    render(
      <ErrorBoundary resetKey="a">
        <div data-testid="ok">ok</div>
      </ErrorBoundary>,
    );
    expect(screen.getByTestId("ok")).toBeTruthy();
  });

  function Recovering() {
    const [resetKey, setResetKey] = useState("a");
    const [broken, setBroken] = useState(true);
    return (
      <>
        <button
          onClick={() => {
            setBroken(false);
            setResetKey("b");
          }}
        >
          fix
        </button>
        <ErrorBoundary resetKey={resetKey} fallback={<div data-testid="fb">failed</div>}>
          {broken ? <Boom /> : <div data-testid="recovered">recovered</div>}
        </ErrorBoundary>
      </>
    );
  }

  it("clears hasError when resetKey changes", () => {
    const spy = vi.spyOn(console, "error").mockImplementation(() => {});
    render(<Recovering />);
    expect(screen.getByTestId("fb")).toBeTruthy();
    // React 19 下必须包 act，否则状态更新不会同步刷新断言。
    act(() => {
      screen.getByText("fix").click();
    });
    expect(screen.getByTestId("recovered")).toBeTruthy();
    spy.mockRestore();
  });

  it("does NOT clear hasError while resetKey stays the same", () => {
    const spy = vi.spyOn(console, "error").mockImplementation(() => {});
    const { rerender } = render(
      <ErrorBoundary resetKey="same" fallback={<div data-testid="fb">failed</div>}>
        <Boom />
      </ErrorBoundary>,
    );
    rerender(
      <ErrorBoundary resetKey="same" fallback={<div data-testid="fb">failed</div>}>
        <Boom />
      </ErrorBoundary>,
    );
    expect(screen.getByTestId("fb")).toBeTruthy();
    spy.mockRestore();
  });
});
