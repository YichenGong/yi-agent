import { useEffect, useState } from "react";
import {
  readKanbanSettings,
  writeKanbanSettings,
  pluginErrorKind,
  type KanbanSettings,
  type KanbanWindow,
  type PluginRpc,
} from "../lib/pluginSettings";

const EMPTY_WINDOW: KanbanWindow = {
  days: "Mon-Fri",
  start: "09:00",
  end: "24:00",
  all_day: false,
  max_tasks: 3,
};

/** superpowers-kanban 的插件设置面板：并发窗口 + 推进间隔。 */
export function SuperpowersKanbanPluginSettings({ rpc }: { rpc: PluginRpc }) {
  const [settings, setSettings] = useState<KanbanSettings | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const loaded = await readKanbanSettings(rpc);
        if (!cancelled) setSettings(loaded);
      } catch (e) {
        if (!cancelled) {
          setError(
            pluginErrorKind(e) === "not_running"
              ? "插件未运行，无法读取设置"
              : "无法读取设置",
          );
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [rpc]);

  if (error && settings === null) {
    return <p role="alert" className="p-4 text-xs text-amber-500">{error}</p>;
  }
  if (settings === null) {
    return <p className="p-4 text-xs text-fg-subtle">正在读取设置…</p>;
  }

  const patch = (next: Partial<KanbanSettings>) => setSettings({ ...settings, ...next });
  const patchWindow = (index: number, next: Partial<KanbanWindow>) => {
    const windows = settings.windows.map((w, i) => (i === index ? { ...w, ...next } : w));
    patch({ windows });
  };

  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      await writeKanbanSettings(rpc, settings);
    } catch (e) {
      setError(pluginErrorKind(e) === "other" ? "保存失败" : "插件未运行，保存失败");
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="p-4">
      <label className="flex items-center gap-3 text-sm">
        <span>默认并发上限</span>
        <input
          aria-label="默认并发上限"
          type="number"
          min={1}
          value={settings.default_max_tasks}
          onChange={(e) => patch({ default_max_tasks: Number(e.target.value) })}
        />
      </label>
      <label className="mt-3 flex items-center gap-3 text-sm">
        <span>推进间隔秒数</span>
        <input
          aria-label="推进间隔秒数"
          type="number"
          min={1}
          max={3600}
          value={settings.interval_secs}
          onChange={(e) => patch({ interval_secs: Number(e.target.value) })}
        />
      </label>

      <h3 className="mt-4 text-sm font-medium text-fg">时段并发窗口</h3>
      <table className="mt-2 w-full text-xs">
        <thead>
          <tr>
            <th>星期</th>
            <th>开始</th>
            <th>结束</th>
            <th>上限</th>
            <th />
          </tr>
        </thead>
        <tbody>
          {settings.windows.map((w, index) => (
            <tr key={index}>
              <td>
                <input
                  aria-label={`窗口 ${index} 星期`}
                  value={w.days}
                  onChange={(e) => patchWindow(index, { days: e.target.value })}
                />
              </td>
              <td>
                <input
                  aria-label={`窗口 ${index} 开始`}
                  value={w.start}
                  disabled={w.all_day}
                  onChange={(e) => patchWindow(index, { start: e.target.value })}
                />
              </td>
              <td>
                <input
                  aria-label={`窗口 ${index} 结束`}
                  value={w.end}
                  disabled={w.all_day}
                  onChange={(e) => patchWindow(index, { end: e.target.value })}
                />
              </td>
              <td>
                <input
                  aria-label={`窗口 ${index} 上限`}
                  type="number"
                  min={1}
                  value={w.max_tasks}
                  onChange={(e) => patchWindow(index, { max_tasks: Number(e.target.value) })}
                />
              </td>
              <td>
                <button
                  type="button"
                  aria-label={`删除窗口 ${index}`}
                  onClick={() => patch({ windows: settings.windows.filter((_, i) => i !== index) })}
                >
                  删除
                </button>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      <button
        type="button"
        className="mt-2 text-xs"
        onClick={() => patch({ windows: [...settings.windows, { ...EMPTY_WINDOW }] })}
      >
        添加窗口
      </button>

      <div className="mt-4">
        <button
          type="button"
          disabled={busy}
          onClick={() => void save()}
          className="rounded border border-line px-3 py-1 text-sm"
        >
          保存
        </button>
      </div>
      {error && (
        <p role="alert" className="mt-2 text-xs text-amber-500">
          {error}
        </p>
      )}
    </section>
  );
}
