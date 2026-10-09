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

/** superpowers-kanban 的插件设置面板：并发窗口 + 推进间隔。
 *
 * `project` 是这些设置所属的项目（清单与 daemon socket 都在它下面）。切换项目
 * 必须重新加载；而宿主每次重渲染可能换新的 `rpc` 函数身份（看板 2 秒轮询就在
 * 驱动宿主重渲染），那是**同一个项目**的刷新，绝不能把用户没保存的输入冲掉。
 * 两种「重跑」因此分开处理：只以 `project` 为依赖。 */
export function SuperpowersKanbanPluginSettings({
  rpc,
  project,
}: {
  rpc: PluginRpc;
  project?: string;
}) {
  const [settings, setSettings] = useState<KanbanSettings | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // 接缝进 ref，加载只在 `project` 变化时跑。宿主每次渲染可能换新的函数身份
  // （看板 2 秒轮询就在驱动宿主重渲染），若把它写进依赖，读取会反复重跑并用
  // 服务端值盖掉用户没保存的输入。
  const rpcRef = useRef(rpc);
  rpcRef.current = rpc;

  useEffect(() => {
    let cancelled = false;
    setSettings(null);
    setError(null);
    void (async () => {
      try {
        const loaded = await readKanbanSettings(rpcRef.current, project);
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
    // 只在挂载与切项目时读一次：同项目的重渲染不许重置编辑态。
  }, [project]);

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
      await writeKanbanSettings(rpcRef.current, settings, project);
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
      const loaded = await readKanbanSettings(rpcRef.current, project);
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
      {/*
       * 一行一个窗口，字段用 flex-wrap 排布。原先的 5 列表格在小窗口下总宽超过
       * 面板（固定 about:blank 的 70vw 弹窗里只有 ~300px），右侧的「上限」与
       * 「删除」两列被整列裁掉——面板只 `overflow-y-auto`，横向溢出既看不见也
       * 滚不到，用户因此找不到每行的删除按钮。换行布局把宽度交给容器，任何宽度
       * 下所有控件都在视野内；ARIA 标签（`窗口 N 星期` 等）保持不变。
       */}
      <div className="mt-2 divide-y divide-line rounded border border-line">
        {settings.windows.map((w, index) => (
          <div key={index} className="flex flex-wrap items-center gap-2 p-2 text-xs">
            <label className="flex items-center gap-1">
              <span className="text-fg-subtle">星期</span>
              <input
                aria-label={`窗口 ${index} 星期`}
                className="w-24 rounded border border-line bg-surface px-2 py-0.5"
                value={w.days}
                onChange={(e) => patchWindow(index, { days: e.target.value })}
              />
            </label>
            <label className="flex items-center gap-1">
              <span className="text-fg-subtle">开始</span>
              <input
                aria-label={`窗口 ${index} 开始`}
                className="w-20 rounded border border-line bg-surface px-2 py-0.5"
                value={w.start}
                disabled={w.all_day}
                onChange={(e) => patchWindow(index, { start: e.target.value })}
              />
            </label>
            <label className="flex items-center gap-1">
              <span className="text-fg-subtle">结束</span>
              <input
                aria-label={`窗口 ${index} 结束`}
                className="w-20 rounded border border-line bg-surface px-2 py-0.5"
                value={w.end}
                disabled={w.all_day}
                onChange={(e) => patchWindow(index, { end: e.target.value })}
              />
            </label>
            <label className="flex items-center gap-1">
              <span className="text-fg-subtle">上限</span>
              <input
                aria-label={`窗口 ${index} 上限`}
                className="w-16 rounded border border-line bg-surface px-2 py-0.5"
                type="number"
                min={1}
                value={w.max_tasks}
                onChange={(e) => {
                  const value = numericInput(e.target.value);
                  if (value !== null) patchWindow(index, { max_tasks: value });
                }}
              />
            </label>
            <button
              type="button"
              aria-label={`删除窗口 ${index}`}
              className="shrink-0 rounded border border-line px-2 py-0.5 text-fg-muted hover:text-fg"
              onClick={() => patch({ windows: settings.windows.filter((_, i) => i !== index) })}
            >
              删除
            </button>
          </div>
        ))}
        {settings.windows.length === 0 && (
          <p className="p-2 text-xs text-fg-subtle">未设置时段窗口</p>
        )}
      </div>
      <button
        type="button"
        className="mt-2 rounded border border-line px-2 py-0.5 text-xs"
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
