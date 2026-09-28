/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, fireEvent, cleanup } from "@testing-library/react";
import { ApprovalBanner } from "./ApprovalBanner";
import type { ApprovalRequest } from "../lib/protocol";

afterEach(cleanup);

const req = (id: string, threadId: string, tool: string): ApprovalRequest => ({
  id,
  params: {
    thread_id: threadId,
    turn_id: "u1",
    request_id: 1,
    tool_name: tool,
    tool_input: {},
    prefix_suggestion: null,
    kind: "Normal",
  },
});

describe("ApprovalBanner", () => {
  it("lists background approvals and jumps to a thread", () => {
    const onJump = vi.fn();
    render(
      <ApprovalBanner items={[req("p1", "t1", "bash")]} onJump={onJump} onDismiss={vi.fn()} />,
    );
    expect(screen.getByText(/bash/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /jump/i }));
    expect(onJump).toHaveBeenCalledWith("t1");
  });

  it("calls onDismiss", () => {
    const onDismiss = vi.fn();
    render(
      <ApprovalBanner items={[req("p1", "t1", "bash")]} onJump={vi.fn()} onDismiss={onDismiss} />,
    );
    fireEvent.click(screen.getByRole("button", { name: /dismiss/i }));
    expect(onDismiss).toHaveBeenCalledTimes(1);
  });
});
