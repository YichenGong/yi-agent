/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { RuntimeActivationNotice } from "./RuntimeActivationNotice";

afterEach(cleanup);

describe("RuntimeActivationNotice", () => {
  it("offers explicit recovery for a failed local activation", () => {
    const retry = vi.fn();
    render(<RuntimeActivationNotice local failed busy={false} onRetry={retry} />);
    fireEvent.click(screen.getByRole("button", { name: "准备运行环境并重试" }));
    expect(retry).toHaveBeenCalledTimes(1);
  });

  it.each([{ local: false, failed: true }, { local: true, failed: false }])(
    "does not offer local recovery for %o", (props) => {
      render(<RuntimeActivationNotice {...props} busy={false} onRetry={vi.fn()} />);
      expect(screen.queryByRole("button")).toBeNull();
    },
  );

  it("prevents repeated recovery while the attempt is busy", () => {
    const retry = vi.fn();
    render(<RuntimeActivationNotice local failed busy onRetry={retry} />);
    const button = screen.getByRole("button", { name: "准备运行环境并重试" });
    expect((button as HTMLButtonElement).disabled).toBe(true);
    fireEvent.click(button);
    expect(retry).not.toHaveBeenCalled();
  });
});
