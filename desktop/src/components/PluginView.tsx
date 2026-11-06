import { useEffect, useRef, useState } from "react";

import type { RpcCall } from "../lib/rpcCall";
import { stateAllowsWrite, type PluginState } from "../hooks/usePlugins";
import { registerPluginConsumer } from "../lib/pluginDemandRegistry";

/**
 * 受管插件视图。
 *
 * 这个组件**只**使用宿主给的受管 view handle（T17 的 `open_plugin_view`），
 * 不自己拼资源 URL，也不 import 插件代码。三条约束：
 *
 * 1. 未启用/未安装时**不创建** iframe：安装前只有静态介绍。
 * 2. `read-only` 时视图仍然渲染（只读也是"能看"），但写入口由宿主桥拒绝，
 *    UI 只负责不暗示"能写"。
 * 3. 视图关闭时必须关掉宿主侧实例，否则它的资源白名单会一直占着。
 */

export type PluginViewState =
  | { kind: "idle" }
  | { kind: "opening" }
  | { kind: "open"; handle: { instance: string; windowLabel: string; url: string } }
  | { kind: "failed"; detail: string };

export interface PluginViewProps {
  plugin: string;
  version: string | null;
  view: string;
  /**
   * The project this view serves.
   *
   * Present so the view can register itself as demand: a window that shows only a
   * plugin has no conversation, so without this nothing would ask for a runtime and
   * the plugin would wait for a service no lease keeps alive.
   */
  project?: string;
  state: PluginState;
  /** 宿主提供的 open/close 命令。缺省时不渲染业务 iframe。 */
  openView?: (args: { plugin: string; version: string; view: string }) => Promise<{
    instance: string;
    windowLabel: string;
    url: string;
  }>;
  closeView?: (instance: string) => Promise<void>;
  /** 嵌入式渲染（测试与内嵌面板）；缺省用宿主窗口。 */
  embedded?: boolean;
}

export function PluginView({ plugin, project, version, view, state, openView, closeView, embedded = false }: PluginViewProps) {
  const [current, setCurrent] = useState<PluginViewState>({ kind: "idle" });
  const handleRef = useRef<string | null>(null);

  // Demand follows the mounted view, not the open window: the service has to be
  // asked for before the handle exists, and it must stop when this view goes away.
  //
  // Only a state that can actually render a view registers. A plugin that is merely
  // installed-but-disabled has nobody to serve, and claiming demand for it would keep
  // a runtime alive for a project with no work in it.
  //
  // The consumer name is per (project, plugin, view), so a second view of the same
  // plugin registers separately and neither can withdraw the other's demand.
  const servable =
    state.kind === "running" || state.kind === "read-only" || state.kind === "failing";
  useEffect(() => {
    if (!project || !servable) return;
    const consumer = `view:${project}:${plugin}:${view}`;
    const release = registerPluginConsumer({ project, plugin, consumer });
    return release;
  }, [project, plugin, view, servable]);

  // 状态不再是"可渲染"时，收掉已经打开的视图。项目切换、停用、卸载都走这里。
  useEffect(() => {
    if (!openView || !version) return;
    if (state.kind !== "running" && state.kind !== "read-only" && state.kind !== "failing") {
      return;
    }
    let cancelled = false;
    setCurrent({ kind: "opening" });
    void (async () => {
      try {
        const handle = await openView({ plugin, version, view });
        if (cancelled) {
          // 打开期间状态变了：立刻关掉，别留一个孤儿窗口。
          await closeView?.(handle.instance).catch(() => {});
          return;
        }
        handleRef.current = handle.instance;
        setCurrent({ kind: "open", handle });
      } catch (error) {
        if (!cancelled) {
          setCurrent({ kind: "failed", detail: error instanceof Error ? error.message : String(error) });
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [openView, closeView, plugin, version, view, state.kind]);

  useEffect(
    () => () => {
      const instance = handleRef.current;
      if (instance) {
        handleRef.current = null;
        void closeView?.(instance).catch(() => {});
      }
    },
    [closeView],
  );

  if (state.kind === "not-installed") {
    return (
      <div data-view-state="not-installed" className="p-4 text-sm text-fg-muted">
        {plugin} 还没有安装。这里只显示介绍，不会加载插件页面。
      </div>
    );
  }

  if (state.kind === "installed-disabled") {
    return (
      <div data-view-state="installed-disabled" className="p-4 text-sm text-fg-muted">
        {plugin} 已安装但未在当前项目启用。启用之前不会加载它的页面。
      </div>
    );
  }

  if (state.kind === "update-waiting" || state.kind === "pending-cleanup") {
    return (
      <div data-view-state={state.kind} className="p-4 text-sm text-amber-500">
        {state.kind === "update-waiting"
          ? "更新在等待你确认新增能力；确认前不会切换运行版本。"
          : "插件待清理：项目不可达，卸载没有完成。"}
      </div>
    );
  }

  if (current.kind === "failed") {
    return (
      <div role="alert" data-view-state="failed" className="p-4 text-sm text-amber-500">
        视图打不开：{current.detail}
      </div>
    );
  }

  if (!embedded) {
    return (
      <div data-view-state={current.kind} className="p-4 text-sm text-fg-muted">
        已在独立窗口中打开 {plugin}。
      </div>
    );
  }

  if (current.kind !== "open") {
    return (
      <div data-view-state="opening" className="p-4 text-sm text-fg-subtle">
        正在打开视图…
      </div>
    );
  }

  return (
    <div data-view-state="open" data-writable={stateAllowsWrite(state) ? "true" : "false"}>
      <iframe
        title={`${plugin} ${view}`}
        src={current.handle.url}
        // 沙箱只给最小权限：插件页面不用弹窗、不用顶层导航、不用表单外送。
        sandbox="allow-scripts allow-same-origin"
        referrerPolicy="no-referrer"
        className="h-full w-full border-0"
      />
    </div>
  );
}

/**
 * 宿主的 open/close 接缝。
 *
 * 刻意做成独立的工厂函数而不是直接 import `@tauri-apps/api`：组件测试不需要
 * 拉起 Tauri，而"视图命令叫什么"是宿主的事，不该散落在组件里。
 */
export function hostViewCommands(invoke: (command: string, args?: Record<string, unknown>) => Promise<unknown>) {
  return {
    openView: async (args: { plugin: string; version: string; view: string }) => {
      const handle = (await invoke("open_plugin_view", args)) as {
        instance: string;
        windowLabel: string;
        url: string;
      };
      return handle;
    },
    closeView: async (instance: string) => {
      await invoke("close_plugin_view", { instance });
    },
  };
}

/** 供测试断言：这个 RPC 接缝没有业务方法可调。 */
export function assertNoBusinessChannel(call: RpcCall | undefined): boolean {
  return call === undefined;
}
