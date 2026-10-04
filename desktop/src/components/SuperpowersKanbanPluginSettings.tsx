import { useEffect, useRef, useState } from "react";
import { formatError } from "../lib/errorMessage";
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

/** 数字输入的守卫：清空输入会让 `Number("")` 变成 0，其它乱输入变成 `NaN`。
 * 两种都不写进 state（保留上一个值），避免保存时送 `NaN`/`0` 出去。 */
function numericInput(raw: string): number | null {
  if (raw.trim() === "") return null;
  const parsed = Number(raw);
  return Number.isFinite(parsed) ? parsed : null;
}

/** superpowers-kanban 的插件设置面板：并发窗口 + 推进间隔。 */
export function SuperpowersKanbanPluginSettings({ rpc }: { rpc: PluginRpc }) {
  const [settings, setSettings] = useState<KanbanSettings | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // 接缝进 ref，加载只在挂载时跑一次。宿主每次渲染可能换新的函数身份
  // （看板 2 秒轮询就在驱动宿主重渲染），若把它写进依赖，读取会反复重跑并用
  // 服务端值盖掉用户没保存的输入。
  const rpcRef = useRef(rpc);
  rpcRef.current = rpc;

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const loaded = await readKanbanSettings(rpcRef.current);
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
    // 只在挂载时读一次：编辑态不许被外来的重渲染重置。
  }, []);

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
      await writeKanbanSettings(rpcRef.current, settings);
    } catch (e) {
      // 插件自己拒绝时把它给的理由原样展示；只有「插件真的没在跑」才用
      // 那句笼统文案。用户输入一概不动（没有乐观更新）。
      setError(
        pluginErrorKind(e) === "not_running" ? "插件未运行，保存失败" : formatError(e),
      );
      return;
    } finally {
      setBusy(false);
    }
    // 写成功后以前端重读的结果为准（规范 §7.3）：插件会规范化（星期压缩、
    // 时段渲染），不重读显示值会和磁盘分叉。重读失败只报错，绝不用空/旧值
    // 盖掉用户输入。
    try {
      const loaded = await readKanbanSettings(rpcRef.current);
      setSettings(loaded);
    } catch (e) {
      setError(formatError(e));
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
          onChange={(e) => {
            const value = numericInput(e.target.value);
            if (value !== null) patch({ default_max_tasks: value });
          }}
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
          onChange={(e) => {
            const value = numericInput(e.target.value);
            if (value !== null) patch({ interval_secs: value });
          }}
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
                  onChange={(e) => {
                    const value = numericInput(e.target.value);
                    if (value !== null) patchWindow(index, { max_tasks: value });
                  }}
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
