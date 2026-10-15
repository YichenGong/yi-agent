/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, fireEvent, cleanup, screen } from "@testing-library/react";
import { ProcessRail } from "./ProcessRail";
import type { ProcessInfo, ProcessStatus } from "../lib/protocol";

afterEach(cleanup);

const p = (id: string, state: string, name?: string): ProcessInfo => ({
  process_id: id,
  name: name ?? null,
  pid: 4242,
  command: "npm run dev",
  cwd: "/tmp",
  status: { state } as ProcessStatus,
  ready: state === "ready",
  on_exit: "kill",
  exit_code: null,
  elapsed_sec: 1.5,
});

describe("ProcessRail", () => {
  it("renders one card per active process with name, pid and status", () => {
    render(<ProcessRail processes={[p("proc_1", "running", "dev")]} onOpen={vi.fn()} />);
    expect(screen.getByText("dev")).toBeTruthy();
    expect(screen.getByText(/4242/)).toBeTruthy();
    expect(screen.getByText("[运行中]")).toBeTruthy();
    expect(screen.getByText(/1\.5s/)).toBeTruthy();
  });

  it("shows an empty state when there are no processes at all", () => {
    render(<ProcessRail processes={[]} onOpen={vi.fn()} />);
    expect(screen.getByText("暂无后台进程")).toBeTruthy();
  });

  it("falls back to the process id when there is no name", () => {
    render(<ProcessRail processes={[p("proc_9", "running")]} onOpen={vi.fn()} />);
    expect(screen.getByText("proc_9")).toBeTruthy();
  });

  it("collapses terminal processes into an expandable group", () => {
    render(
      <ProcessRail
        processes={[p("proc_1", "running", "dev"), p("proc_2", "exited", "old-job")]}
        onOpen={vi.fn()}
      />,
    );
    // 终态默认不出现在卡片区,只以一个分组标题暴露计数。
    expect(screen.queryByText("old-job")).toBeNull();
    expect(screen.getByText(/已结束.*1/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /已结束/ }));
    expect(screen.getByText("old-job")).toBeTruthy();
  });

  it("invokes onOpen with the process id when a card is clicked", () => {
    const onOpen = vi.fn();
    render(<ProcessRail processes={[p("proc_7", "running", "dev")]} onOpen={onOpen} />);
    fireEvent.click(screen.getByRole("button", { name: /查看进程 dev/ }));
    expect(onOpen).toHaveBeenCalledWith("proc_7");
  });

  it("marks the process the user has opened", () => {
    render(
      <ProcessRail
        processes={[p("proc_1", "running", "a"), p("proc_2", "running", "b")]}
        onOpen={vi.fn()}
        selectedProcessId="proc_2"
      />,
    );
    expect(
      screen.getByRole("button", { name: /查看进程 b/ }).getAttribute("aria-current"),
    ).toBe("true");
    expect(
      screen.getByRole("button", { name: /查看进程 a/ }).getAttribute("aria-current"),
    ).toBeNull();
  });
});