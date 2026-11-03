/**
 * 受管插件的桌面侧状态模型。
 *
 * 关键决定：**不用一个 `enabled` 布尔表示插件状态**。真实状态有七个，彼此
 * 不可合并（计划 T19 Step 1 明确要求逐个独立）：
 *
 * | 状态 | 含义 |
 * |---|---|
 * | `not-installed` | 目录里有，但本机没装 |
 * | `installed-disabled` | 装了，但当前项目没授权 |
 * | `read-only` | 只授权了 Read：能看，不能写 |
 * | `running` | 授权且当前有已提交的激活代次 |
 * | `failing` | 授权在，但运行/健康检查失败 |
 * | `update-waiting` | 能力增加的更新在等用户重新确认 |
 * | `pending-cleanup` | 卸载时项目不可达，留着待清理记录 |
 *
 * 把 `read-only` 折进 `enabled=true` 会直接导致"只读"被说成"能用"——协议
 * spec §5.3 明确禁止这种措辞，因为它只限制桥调用，不限制原生文件访问。
 */

import { RPC } from "../lib/routes";
import type { RpcCall } from "../lib/rpcCall";

/** 与 `PluginSummary`（旧 supervisor 协议）不是同一个东西，别混用。 */
export interface ManagedPluginRow {
  plugin: string;
  selected: string | null;
  installed: string[];
  /** 已授权的项目句柄。 */
  projects: string[];
}

export type PluginState =
  | { kind: "not-installed"; plugin: string; versions: string[] }
  | { kind: "installed-disabled"; plugin: string; version: string | null }
  | { kind: "read-only"; plugin: string; version: string | null; project: string }
  | { kind: "running"; plugin: string; version: string | null; project: string; activation: number }
  | { kind: "failing"; plugin: string; version: string | null; detail: string }
  | { kind: "update-waiting"; plugin: string; version: string | null; requires: string[] }
  | { kind: "pending-cleanup"; plugin: string; project: string; detail: string };

export interface TransactionSnapshot {
  phase?: string;
  /** 这次更新新增需要确认的能力。 */
  requiresConfirmation?: string[];
  /** 当前项目实际获批的能力。 */
  granted?: string[];
  activation?: number;
}

export interface HealthSnapshot {
  phase?: "failing" | "pending-cleanup";
  project?: string;
  detail?: string;
}

export interface PluginCatalogOptions {
  /** 当前项目句柄（`ph1_...`），用于判定授权状态。 */
  project?: string;
  transaction?: TransactionSnapshot;
  health?: HealthSnapshot;
}

/**
 * 从管理 API 读出的原始行推导状态。**纯函数**：所有分支都能单测，
 * 不需要挂一个 React 树。
 */
export function deriveState(row: ManagedPluginRow, options: PluginCatalogOptions = {}): PluginState {
  const project = options.project ?? "";
  const installed = row.installed.length > 0;

  if (!installed) {
    return { kind: "not-installed", plugin: row.plugin, versions: [] };
  }
  const version = row.selected ?? row.installed[row.installed.length - 1] ?? null;

  if (options.health?.phase === "pending-cleanup") {
    return {
      kind: "pending-cleanup",
      plugin: row.plugin,
      project: options.health.project ?? project,
      detail: options.health.detail ?? "项目不可达，卸载未完成",
    };
  }

  if (options.transaction?.phase === "draining" && options.transaction.requiresConfirmation?.length) {
    return {
      kind: "update-waiting",
      plugin: row.plugin,
      version,
      requires: options.transaction.requiresConfirmation,
    };
  }

  if (!project || !row.projects.includes(project)) {
    return { kind: "installed-disabled", plugin: row.plugin, version };
  }

  if (options.health?.phase === "failing") {
    return {
      kind: "failing",
      plugin: row.plugin,
      version,
      detail: options.health.detail ?? "运行检查失败",
    };
  }

  const granted = options.transaction?.granted ?? [];
  const onlyRead = granted.length > 0 && granted.every((capability) => capability === "Read");
  if (onlyRead) {
    return { kind: "read-only", plugin: row.plugin, version, project };
  }

  return {
    kind: "running",
    plugin: row.plugin,
    version,
    project,
    activation: options.transaction?.activation ?? 1,
  };
}

/** 一个状态的中文标题。UI 与测试共用同一份措辞。 */
export function stateLabel(state: PluginState): string {
  switch (state.kind) {
    case "not-installed":
      return "未安装";
    case "installed-disabled":
      return "已安装，未在本项目启用";
    case "read-only":
      return "只读启用";
    case "running":
      return "运行中";
    case "failing":
      return "运行故障";
    case "update-waiting":
      return "更新等待确认";
    case "pending-cleanup":
      return "待清理";
  }
}

/** 这个状态是否允许渲染业务视图（iframe）。 */
export function stateRendersView(state: PluginState): boolean {
  return state.kind === "running" || state.kind === "read-only" || state.kind === "failing";
}

/** 这个状态是否允许写。`read-only` 明确不允许。 */
export function stateAllowsWrite(state: PluginState): boolean {
  return state.kind === "running";
}

/**
 * 读全部已安装插件，**不按项目过滤**。
 *
 * 按项目过滤会把"装了但没在这个项目启用"的行一起滤掉，于是 UI 只能显示
 * "未安装"或干脆空白——用户看不到它、也就没法去启用它。项目只用于**推导
 * 状态**（`deriveState`），不用于裁剪列表。
 */
export async function loadManagedPlugins(call: RpcCall): Promise<ManagedPluginRow[]> {
  const result = await call<{ plugins?: unknown }>(RPC.pluginsV1List, {});
  const rows = Array.isArray(result?.plugins) ? result.plugins : [];
  return rows
    .filter((row): row is Record<string, unknown> => typeof row === "object" && row !== null)
    .filter((row) => typeof row.plugin === "string")
    .map((row) => ({
      plugin: row.plugin as string,
      selected: typeof row.selected === "string" ? row.selected : null,
      installed: Array.isArray(row.installed)
        ? row.installed.filter((version): version is string => typeof version === "string")
        : [],
      projects: Array.isArray(row.projects)
        ? row.projects.filter((project): project is string => typeof project === "string")
        : [],
    }));
}

export interface CapabilityChoice {
  /** 用户勾选的能力。必需项必须始终包含。 */
  granted: string[];
}

/**
 * 授权变更的请求体。
 *
 * `granted` 是**分项**能力列表，不是一个 `enabled` 布尔。只读授权就是
 * `granted=["Read"]`；它不阻止原生文件访问，UI 文案必须说清楚。
 */
export async function changeGrant(
  call: RpcCall,
  params: { plugin: string; project: string; granted: string[] },
): Promise<void> {
  await call(RPC.pluginsV1ChangeGrant, {
    plugin: params.plugin,
    project: params.project,
    capabilities: params.granted,
  });
}

/** 启用某项目。安装按钮**不得**顺带调用它（计划 T19 Step 1）。 */
export async function enableProject(
  call: RpcCall,
  params: { plugin: string; project: string; granted: string[] },
): Promise<void> {
  await call(RPC.pluginsV1EnableProject, {
    plugin: params.plugin,
    project: params.project,
    capabilities: params.granted,
  });
}

export async function disableProject(
  call: RpcCall,
  params: { plugin: string; project: string },
): Promise<void> {
  await call(RPC.pluginsV1DisableProject, { plugin: params.plugin, project: params.project });
}

export async function readTransaction(call: RpcCall, plugin: string): Promise<TransactionSnapshot> {
  const result = await call<{ phase?: string; admissionOpen?: boolean }>(RPC.pluginsV1Transaction, {
    plugin,
  });
  return { phase: result?.phase };
}
