import { useEffect, useState, type ComponentType } from "react";
import { listPlugins, type PluginRpc, type PluginSummary } from "../lib/pluginSettings";
import { SuperpowersKanbanPluginSettings } from "./SuperpowersKanbanPluginSettings";

/**
 * 宿主注入的插件 RPC 接缝。
 *
 * 非泛型签名（与 SettingsDialog 的 `pluginCall` 同形）：宿主传进来的与测试里
 * 手写的普通函数都能直接赋值。运行时形状与 `PluginRpc` 一致，缺的只是编译期的
 * 「调用方自选返回类型」，见下面的一次断言。
 */
export type PluginsCall = (method: string, params: unknown) => Promise<unknown>;

/**
 * 插件名 → 设置面板。未注册的插件仍列出来，只是没有面板——不静默消失。
 * 加一个插件 = 往这里加一行。
 */
const REGISTRY: Record<string, ComponentType<{ rpc: PluginRpc }>> = {
  "superpowers-kanban": SuperpowersKanbanPluginSettings,
};

export function SettingsPluginsTab({ call }: { call?: PluginsCall }) {
  const [plugins, setPlugins] = useState<PluginSummary[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!call) {
      setPlugins([]);
      return;
    }
    // 接缝在运行时就是 PluginRpc 的形状；这里补回调用方自选返回类型的泛型能力。
    const rpc = call as PluginRpc;
    let cancelled = false;
    void (async () => {
      try {
        const loaded = await listPlugins(rpc);
        if (!cancelled) setPlugins(loaded);
      } catch {
        if (!cancelled) setError("无法读取插件列表");
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [call]);

  if (error) {
    return <p role="alert" className="p-4 text-xs text-amber-500">{error}</p>;
  }
  if (plugins === null) {
    return <p className="p-4 text-xs text-fg-subtle">正在读取插件…</p>;
  }
  if (plugins.length === 0) {
    return <p className="p-4 text-sm text-fg-muted">未安装任何插件</p>;
  }

  return (
    <div className="divide-y divide-line">
      {plugins.map((plugin) => {
        const Panel = REGISTRY[plugin.name];
        return (
          <section key={plugin.name} className="p-4">
            <h2 className="text-sm font-medium text-fg">{plugin.name}</h2>
            {Panel ? (
              <Panel rpc={call as PluginRpc} />
            ) : (
              <p className="mt-2 text-xs text-fg-subtle">该插件暂无可配置项</p>
            )}
          </section>
        );
      })}
    </div>
  );
}
