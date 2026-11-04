import { describe, it, expect } from "vitest";
import {
  formatElapsed,
  isActive,
  partitionProcesses,
  processDisplayName,
  requestsIdleSleepPrevention,
  statusColor,
  statusLabel,
} from "./processes";
import type { ProcessInfo, ProcessStatus } from "./protocol";

const st = (state: string): ProcessStatus => ({ state });

const p = (id: string, state: string, name?: string): ProcessInfo => ({
  process_id: id,
  name: name ?? null,
  pid: 4242,
  command: "npm run dev",
  cwd: "/tmp",
  status: st(state),
  ready: state === "ready",
  on_exit: "kill",
  exit_code: null,
  elapsed_sec: 1.5,
});

describe("processes folding", () => {
  it("treats starting/running/ready as active", () => {
    expect(isActive(st("starting"))).toBe(true);
    expect(isActive(st("running"))).toBe(true);
    expect(isActive(st("ready"))).toBe(true);
  });

  it("treats exited/killed/failed_to_start as terminal", () => {
    expect(isActive(st("exited"))).toBe(false);
    expect(isActive(st("killed"))).toBe(false);
    expect(isActive(st("failed_to_start"))).toBe(false);
  });

  it("reads the state out of the wire object, not the object itself", () => {
    // 线协议是 `{"state":"running"}`；若把它当字符串用，这里会全判成终态。
    expect(isActive({ state: "running", code: null })).toBe(true);
  });

  it("splits a list into active and finished, keeping order", () => {
    const { active, finished } = partitionProcesses([
      p("proc_1", "running", "dev"),
      p("proc_2", "exited"),
      p("proc_3", "ready", "server"),
      p("proc_4", "killed"),
    ]);
    expect(active.map((x) => x.process_id)).toEqual(["proc_1", "proc_3"]);
    expect(finished.map((x) => x.process_id)).toEqual(["proc_2", "proc_4"]);
  });

  it("falls back to the process id when there is no name", () => {
    expect(processDisplayName(p("proc_7", "running"))).toBe("proc_7");
    expect(processDisplayName(p("proc_8", "running", "dev"))).toBe("dev");
  });

  it("maps every status to a distinct color and label", () => {
    expect(statusColor(st("running"))).toContain("amber");
    expect(statusColor(st("ready"))).toContain("emerald");
    expect(statusColor(st("exited"))).toContain("fg-subtle");
    expect(statusColor(st("killed"))).toContain("red");
    expect(statusLabel(st("failed_to_start"))).toBe("失败");
  });

  it("degrades gracefully on an unknown state instead of crashing", () => {
    expect(statusLabel({ state: "some_future_state" })).toBe("some_future_state");
    expect(isActive({ state: "some_future_state" })).toBe(false);
  });

  it("formats elapsed seconds with one decimal", () => {
    expect(formatElapsed(1.54)).toBe("1.5s");
    expect(formatElapsed(0)).toBe("0.0s");
  });
});
describe("prevent_idle_sleep backward compatibility", () => {
  it("reads a missing field (old sidecar) as false", () => {
    // 原有无字段 fixture：旧 sidecar 不返回该键。
    const legacy = p("p1", "running");
    expect(legacy.prevent_idle_sleep).toBeUndefined();
    expect(requestsIdleSleepPrevention(legacy)).toBe(false);
  });

  it("keeps explicit false and true distinct", () => {
    expect(
      requestsIdleSleepPrevention({ ...p("p1", "running"), prevent_idle_sleep: false }),
    ).toBe(false);
    expect(
      requestsIdleSleepPrevention({ ...p("p1", "running"), prevent_idle_sleep: true }),
    ).toBe(true);
  });

  it("carries true and false through the partition unchanged", () => {
    const optedIn = { ...p("p1", "running"), prevent_idle_sleep: true };
    const optedOut = { ...p("p2", "running"), prevent_idle_sleep: false };
    const { active } = partitionProcesses([optedIn, optedOut]);
    expect(active.map(requestsIdleSleepPrevention)).toEqual([true, false]);
    // 原显示契约不变：同一个 state 仍然归到同一组、同一个名字。
    expect(active.map((info) => processDisplayName(info))).toEqual(["p1", "p2"]);
  });

  it("reads process/read results with the same rule", () => {
    const legacyRead = {
      process_id: "p1",
      name: null,
      stdout: "",
      stderr: "",
      next_cursor: 0,
      truncated: false,
      status: st("running"),
      ready: true,
    };
    expect(requestsIdleSleepPrevention(legacyRead)).toBe(false);
    expect(
      requestsIdleSleepPrevention({ ...legacyRead, prevent_idle_sleep: true }),
    ).toBe(true);
  });
});
