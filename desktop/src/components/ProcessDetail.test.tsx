/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, fireEvent, cleanup, screen, waitFor } from "@testing-library/react";
import { ProcessDetail } from "./ProcessDetail";
import type { ProcessInfo } from "../lib/protocol";

afterEach(cleanup);

const info = (state: string = "running"): ProcessInfo => ({
  process_id: "proc_1",
  name: "dev",
  pid: 4242,
  command: "npm run dev",
  cwd: "/tmp",
  status: { state },
  ready: state === "ready",
  on_exit: "kill",
  exit_code: state === "exited" ? 0 : null,
  elapsed_sec: 3.5,
});

const output = { stdout: "ready on 3000\n", stderr: "warn: slow\n", truncated: false };

describe("ProcessDetail", () => {
  it("renders status, command and both output panes", () => {
    render(<ProcessDetail info={info()} output={output} onClose={vi.fn()} onKill={vi.fn()} />);
    expect(screen.getByText("dev")).toBeTruthy();
    expect(screen.getByText("npm run dev")).toBeTruthy();
    expect(screen.getByText(/ready on 3000/)).toBeTruthy();
    expect(screen.getByText(/warn: slow/)).toBeTruthy();
  });

  it("warns when earlier output has scrolled out of the buffer", () => {
    render(
      <ProcessDetail
        info={info()}
        output={{ ...output, truncated: true }}
        onClose={vi.fn()}
        onKill={vi.fn()}
      />,
    );
    expect(screen.getByText("较早输出已滚出")).toBeTruthy();
  });

  it("shows an empty state before the first read", () => {
    render(<ProcessDetail info={info()} output={null} onClose={vi.fn()} onKill={vi.fn()} />);
    expect(screen.getAllByText("(空)").length).toBe(2);
  });

  it("asks for confirmation before killing, and cancels on Escape", () => {
    const onKill = vi.fn();
    render(<ProcessDetail info={info()} output={output} onClose={vi.fn()} onKill={onKill} />);
    fireEvent.click(screen.getByRole("button", { name: "终止进程" }));
    expect(screen.getByRole("dialog")).toBeTruthy();
    fireEvent.keyDown(screen.getByRole("dialog"), { key: "Escape" });
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(onKill).not.toHaveBeenCalled();
  });

  it("calls onKill once even if confirm is clicked twice", async () => {
    const onKill = vi.fn().mockResolvedValue(undefined);
    render(<ProcessDetail info={info()} output={output} onClose={vi.fn()} onKill={onKill} />);
    fireEvent.click(screen.getByRole("button", { name: "终止进程" }));
    const confirm = screen.getByRole("button", { name: "确认终止" });
    fireEvent.click(confirm);
    fireEvent.click(confirm);
    await waitFor(() => expect(onKill).toHaveBeenCalledTimes(1));
  });

  it("hides the kill affordance for a terminal process", () => {
    render(
      <ProcessDetail info={info("exited")} output={output} onClose={vi.fn()} onKill={vi.fn()} />,
    );
    expect(screen.queryByRole("button", { name: "终止进程" })).toBeNull();
    expect(screen.getByText(/退出码 0/)).toBeTruthy();
  });

  it("pauses following when the user scrolls up, and resumes at the bottom", () => {
    render(<ProcessDetail info={info()} output={output} onClose={vi.fn()} onKill={vi.fn()} />);
    const stdout = screen.getByTestId("process-stdout");
    // jsdom 不做布局:显式给尺寸与滚动位置,模拟"用户上滚"。
    Object.defineProperty(stdout, "scrollHeight", { value: 400, configurable: true });
    Object.defineProperty(stdout, "clientHeight", { value: 100, configurable: true });
    stdout.scrollTop = 100;
    fireEvent.scroll(stdout);
    expect(screen.getByText("已暂停追尾")).toBeTruthy();
    stdout.scrollTop = 300;
    fireEvent.scroll(stdout);
    expect(screen.queryByText("已暂停追尾")).toBeNull();
  });
});