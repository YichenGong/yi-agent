/**
 * 受管后台进程的展示层折叠。
 *
 * 纯函数 + 常量,与组件分开:分组的对错是状态 bug,渲染的对错是显示 bug,
 * 两者该分开测(与 `subagents.ts` 的理由相同)。
 *
 * 状态一律经 `status.state` 读取——线协议是 `{"state":"running"}`,不是裸字符串
 * (见 `protocol.ts` 的 `ProcessStatus`)。
 */
import type { ProcessInfo, ProcessStatus } from "./protocol";

/** 仍在推进、值得默认显示的三个状态。 */
const ACTIVE = ["starting", "running", "ready"];

export function isActive(status: ProcessStatus): boolean {
  return ACTIVE.includes(status.state);
}

/**
 * 把完整列表拆成「活跃」与「已结束」两组,各自保持后端给的顺序。
 *
 * 后端 `ProcessManager::list()` 返回全部条目(只在启动失败或关闭时移除),
 * 长对话里终态会持续累积;分组放在展示层,后端不加 retention 语义。
 */
export function partitionProcesses(processes: ProcessInfo[]): {
  active: ProcessInfo[];
  finished: ProcessInfo[];
} {
  const active: ProcessInfo[] = [];
  const finished: ProcessInfo[] = [];
  for (const p of processes) {
    // 非活跃即终态:exited / killed / failed_to_start 由 else 分支覆盖,
    // 无需再列一遍,少一处会漂移的枚举。
    (isActive(p.status) ? active : finished).push(p);
  }
  return { active, finished };
}

export function processDisplayName(p: ProcessInfo): string {
  return p.name && p.name.length > 0 ? p.name : p.process_id;
}

export function statusColor(status: ProcessStatus): string {
  switch (status.state) {
    case "ready":
      return "text-emerald-400";
    case "exited":
      return "text-fg-subtle";
    case "killed":
    case "failed_to_start":
      return "text-red-400";
    default:
      return "text-amber-400";
  }
}

const LABELS: Record<string, string> = {
  starting: "启动中",
  running: "运行中",
  ready: "就绪",
  exited: "已退出",
  killed: "已终止",
  failed_to_start: "失败",
};

export function statusLabel(status: ProcessStatus): string {
  // 未知状态原样回显:后端将来加状态时不至于显示成空白,也不抛错。
  return LABELS[status.state] ?? status.state;
}

export function formatElapsed(sec: number): string {
  return `${sec.toFixed(1)}s`;
}