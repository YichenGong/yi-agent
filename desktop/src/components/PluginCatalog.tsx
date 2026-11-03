import { useEffect, useState } from "react";

import type { RpcCall } from "../lib/rpcCall";
import {
  changeGrant,
  deriveState,
  disableProject,
  enableProject,
  loadManagedPlugins,
  stateAllowsWrite,
  stateLabel,
  stateRendersView,
  type CapabilityChoice,
  type ManagedPluginRow,
  type PluginState,
} from "../hooks/usePlugins";

/**
 * 受管插件目录。
 *
 * 这个组件**只**调用管理 API（`plugins/v1/*`），不碰业务方法，也不知道任何
 * 具体插件的名字——加一个插件不需要改这里。
 *
 * 三条来自计划 T19 的硬约束：
 *
 * 1. 每个状态独立展示，不用一个 `enabled` 布尔糊住。
 * 2. 「安装」不得隐式开常驻（`Resident`）或启用项目：安装与授权是两步。
 * 3. 安装前只渲染静态介绍，**不创建**业务 iframe。
 */

export interface PluginCatalogProps {
  call?: RpcCall;
  /** 当前项目句柄；缺失时只能展示安装状态，不能启用。 */
  project?: string;
  /** 打开某个已启用插件的受管视图。缺省时打开按钮不渲染。 */
  onOpenView?: (plugin: string, view: string) => void;
  /** 每个插件声明的能力；目录侧本该从清单读，这里由调用方注入以免伪造。 */
  declarations?: Record<string, { required: string[]; optional: string[]; settingsView?: boolean }>;
  /** 事务与健康快照，键是插件 id。 */
  transactions?: Record<string, { phase?: string; requiresConfirmation?: string[]; granted?: string[]; activation?: number }>;
}

interface Row {
  plugin: string;
  version: string | null;
  state: PluginState;
  required: string[];
  optional: string[];
  settingsView: boolean;
}

export function PluginCatalog({ call, project, onOpenView, declarations = {}, transactions = {} }: PluginCatalogProps) {
  // 声明与事务快照在 React 里每次渲染都可能是新对象字面量。把它们折叠成一个
  // 稳定键再进依赖，否则 effect 每渲染一次就重跑一次（并且再次 setState），
  // 表现为看板一直"正在读取插件…"。
  const declarationsKey = JSON.stringify(declarations);
  const transactionsKey = JSON.stringify(transactions);

  const [rows, setRows] = useState<Row[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  useEffect(() => {
    if (!call) {
      setRows([]);
      return;
    }
    let cancelled = false;
    void (async () => {
      try {
        const loaded: ManagedPluginRow[] = await loadManagedPlugins(call);
        if (cancelled) return;
        setRows(
          loaded.map((row) => {
            const declaration = declarations[row.plugin] ?? { required: ["Read"], optional: [] };
            return {
              plugin: row.plugin,
              version: row.selected ?? row.installed[row.installed.length - 1] ?? null,
              state: deriveState(row, { project, transaction: transactions[row.plugin] }),
              required: declaration.required,
              optional: declaration.optional,
              settingsView: declaration.settingsView === true,
            };
          }),
        );
      } catch {
        if (!cancelled) setError("无法读取受管插件列表");
      }
    })();
    return () => {
      cancelled = true;
    };
    // declarations / transactions 通过 JSON 键参与依赖，避免对象身份抖动。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [call, project, declarationsKey, transactionsKey]);

  if (error) {
    return (
      <p role="alert" className="p-4 text-xs text-amber-500">
        {error}
      </p>
    );
  }
  if (rows === null) {
    return <p className="p-4 text-xs text-fg-subtle">正在读取插件…</p>;
  }
  if (rows.length === 0) {
    return <p className="p-4 text-sm text-fg-muted">没有可管理的插件</p>;
  }

  const act = async (plugin: string, work: () => Promise<void>) => {
    setBusy(plugin);
    setNotice(null);
    try {
      await work();
      setNotice(`${plugin}：已提交`);
    } catch (failure) {
      setNotice(`${plugin}：${failure instanceof Error ? failure.message : String(failure)}`);
    } finally {
      setBusy(null);
    }
  };

  return (
    <div className="divide-y divide-line">
      {notice && (
        <p role="status" className="px-4 py-2 text-xs text-fg-muted">
          {notice}
        </p>
      )}
      {rows.map((row) => (
        <section key={row.plugin} className="p-4" aria-label={row.plugin}>
          <header className="flex items-baseline justify-between gap-3">
            <h2 className="text-sm font-medium text-fg">{row.plugin}</h2>
            <span data-state={row.state.kind} className="text-xs text-fg-subtle">
              {stateLabel(row.state)}
            </span>
          </header>
          {row.version && (
            <p className="mt-1 font-mono text-xs text-fg-subtle">v{row.version}</p>
          )}

          {/* 安装前只有静态介绍：没有业务 iframe，也就没有业务后端可打。 */}
          <StateBody
            row={row}
            project={project}
            busy={busy === row.plugin}
            onOpenView={onOpenView}
            onEnable={(granted) => {
              if (!call || !project) return;
              void act(row.plugin, () => enableProject(call, { plugin: row.plugin, project, granted }));
            }}
            onChangeGrant={(granted) => {
              if (!call || !project) return;
              void act(row.plugin, () => changeGrant(call, { plugin: row.plugin, project, granted }));
            }}
            onDisable={() => {
              if (!call || !project) return;
              void act(row.plugin, () => disableProject(call, { plugin: row.plugin, project }));
            }}
          />
        </section>
      ))}
    </div>
  );
}

function StateBody({
  row,
  project,
  busy,
  onEnable,
  onChangeGrant,
  onDisable,
  onOpenView,
}: {
  row: Row;
  project?: string;
  busy: boolean;
  onEnable: (granted: string[]) => void;
  onChangeGrant: (granted: string[]) => void;
  onDisable: () => void;
  onOpenView?: (plugin: string, view: string) => void;
}) {
  const choice = useCapabilityChoice(row, row.state.kind === "read-only" ? ["Read"] : row.required, row.state);

  switch (row.state.kind) {
    case "not-installed":
      return (
        <div className="mt-2 space-y-2">
          <p className="text-xs text-fg-muted">
            这个插件还没有安装到本机。安装不会自动为任何项目启用它，也不会顺带开启常驻进程。
          </p>
        </div>
      );

    case "installed-disabled":
      return (
        <div className="mt-2 space-y-2">
          <p className="text-xs text-fg-muted">
            已安装。启用会为当前项目授予下面这些能力；只读授权只限制通过宿主桥的调用，不限制原生文件访问。
          </p>
          {!project ? (
            <p className="text-xs text-amber-500">需要先选择一个项目才能启用。</p>
          ) : (
            <>
              <CapabilityList choice={choice} />
              <button
                type="button"
                disabled={busy}
                onClick={() => onEnable(choice.granted)}
                className="rounded border border-line px-3 py-1 text-xs"
              >
                为当前项目启用
              </button>
            </>
          )}
        </div>
      );

    case "read-only":
      return (
        <div className="mt-2 space-y-2">
          <p className="text-xs text-fg-muted">
            只读启用：可以查看，不能修改业务数据。这不限制原生文件访问。
          </p>
          <div className="flex flex-wrap items-center gap-2">
            {stateRendersView(row.state) && onOpenView && (
              <button
                type="button"
                onClick={() => onOpenView(row.plugin, "main")}
                className="rounded border border-line px-3 py-1 text-xs"
              >
                打开视图
              </button>
            )}
            <button
              type="button"
              disabled={busy}
              onClick={() => onChangeGrant(choice.granted)}
              className="rounded border border-line px-3 py-1 text-xs"
            >
              调整授权
            </button>
            <button
              type="button"
              disabled={busy}
              onClick={onDisable}
              className="rounded border border-line px-3 py-1 text-xs"
            >
              停用
            </button>
          </div>
        </div>
      );

    case "running":
      return (
        <div className="mt-2 space-y-2">
          <p className="text-xs text-fg-muted">正在为当前项目运行。</p>
          <div className="flex flex-wrap items-center gap-2">
            {onOpenView && (
              <button
                type="button"
                onClick={() => onOpenView(row.plugin, "main")}
                className="rounded border border-line px-3 py-1 text-xs"
              >
                打开视图
              </button>
            )}
            {row.settingsView && onOpenView && (
              <button
                type="button"
                onClick={() => onOpenView(row.plugin, "settings")}
                className="rounded border border-line px-3 py-1 text-xs"
              >
                打开设置
              </button>
            )}
            <button
              type="button"
              disabled={busy}
              onClick={onDisable}
              className="rounded border border-line px-3 py-1 text-xs"
            >
              停用
            </button>
          </div>
          {!stateAllowsWrite(row.state) && <p className="text-xs text-amber-500">当前授权不允许写入。</p>}
        </div>
      );

    case "failing":
      return (
        <div className="mt-2 space-y-2">
          <p role="alert" className="text-xs text-amber-500">
            运行故障：{row.state.detail}
          </p>
          <p className="text-xs text-fg-subtle">
            故障期间不执行自动任务。可以停用后重新启用，或在日志里定位原因。
          </p>
          <button
            type="button"
            disabled={busy}
            onClick={onDisable}
            className="rounded border border-line px-3 py-1 text-xs"
          >
            停用
          </button>
        </div>
      );

    case "update-waiting":
      return (
        <div className="mt-2 space-y-2">
          <p className="text-xs text-fg-muted">
            有一个更新需要新增能力：{row.state.requires.join("、")}。确认之前，整个更新保持等待，
            项目不会混跑两个版本。
          </p>
          <CapabilityList choice={choice} />
          <button
            type="button"
            disabled={busy}
            onClick={() => onChangeGrant(choice.granted)}
            className="rounded border border-line px-3 py-1 text-xs"
          >
            确认新增能力
          </button>
        </div>
      );

    case "pending-cleanup":
      return (
        <div className="mt-2 space-y-2">
          <p role="alert" className="text-xs text-amber-500">
            待清理：{row.state.detail}
          </p>
          <p className="text-xs text-fg-subtle">
            项目不可达时不会宣称卸载完成。记录会保留到能确认清理为止。
          </p>
        </div>
      );
  }
}

function CapabilityList({ choice }: { choice: ReturnType<typeof useCapabilityChoice> }) {
  return (
    <fieldset className="space-y-1">
      <legend className="text-xs text-fg-subtle">授予的能力</legend>
      {choice.options.map((capability) => (
        <label key={capability.name} className="flex items-center gap-2 text-xs">
          <input
            type="checkbox"
            aria-label={capability.name}
            checked={capability.granted}
            disabled={capability.required}
            onChange={() => choice.toggle(capability.name)}
          />
          <span>{capability.name}</span>
          {capability.required && <span className="text-fg-subtle">（必需）</span>}
        </label>
      ))}
    </fieldset>
  );
}

/**
 * 分项授权的选择状态。必需能力不可取消；可选能力默认全不选——安装不等于
 * 顺带开常驻。
 */
function useCapabilityChoice(row: Row, initial: string[], state: PluginState) {
  const [granted, setGranted] = useState<string[]>(() => dedupe([...row.required, ...initial]));
  useEffect(() => {
    setGranted(dedupe([...row.required, ...initial]));
    // initial 由外部快照决定，快照变了才重置。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [row.plugin, state.kind]);

  const options = dedupe([...row.required, ...row.optional]).map((name) => ({
    name,
    granted: granted.includes(name),
    required: row.required.includes(name),
  }));

  return {
    options,
    granted,
    toggle: (name: string) => {
      if (row.required.includes(name)) return;
      setGranted((current) =>
        current.includes(name) ? current.filter((entry) => entry !== name) : [...current, name],
      );
    },
  };
}

function dedupe(values: string[]): string[] {
  return [...new Set(values)];
}

/** 供测试与调用方复用：把一次选择收敛成请求体。 */
export function grantRequest(choice: CapabilityChoice): { capabilities: string[] } {
  return { capabilities: dedupe(choice.granted) };
}
