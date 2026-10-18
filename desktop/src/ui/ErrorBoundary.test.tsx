/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { ErrorBoundary } from "./ErrorBoundary";

afterEach(cleanup);

function Boom({ silent }: { silent?: boolean }): never {
  if (!silent) throw new Error("boom");
  throw new Error("boom");
}

describe("ErrorBoundary", () => {
  it("renders its children when nothing throws", () => {
    render(
      <ErrorBoundary>
        <span>ok</span>
      </ErrorBoundary>,
    );
    expect(screen.getByText("ok")).toBeTruthy();
  });

  it("renders the fallback and reports the error when a child throws", () => {
    const onError = vi.fn();
    // React 会把 boundary 捕获到的错误也打到 console.error，这里静音以免刷屏。
    const spy = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      render(
        <ErrorBoundary onError={onError} fallback={<span>fallback</span>}>
          <Boom />
        </ErrorBoundary>,
      );
      expect(screen.getByText("fallback")).toBeTruthy();
      expect(onError).toHaveBeenCalledTimes(1);
    } finally {
      spy.mockRestore();
    }
  });

  it("renders nothing extra when no fallback is given", () => {
    const spy = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      const { container } = render(
        <ErrorBoundary>
          <Boom />
        </ErrorBoundary>,
      );
      expect(container.textContent).toBe("");
    } finally {
      spy.mockRestore();
    }
  });
});